mod agent;
mod compact;
mod git;
mod llm;
mod picker;
mod providers;
mod redact;
mod render;
mod serve;
mod session;
mod stats;
mod tasks;
mod ui;
mod worker;

use anyhow::{Context, Result, bail, ensure};
use rustyline::error::ReadlineError;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The build `low --version` names and the session log records.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " ", env!("LOW_BUILD"));
const ABOUT: &str = "A low-entropy, code-mode coding agent. Without a task it starts a REPL (/models, /model [filter], /effort [level], /providers, /compact, /cost, /think).";
const USAGE: &str = "Usage: low [OPTIONS] [TASK]";
const HELP: &str = "\
Arguments:
  [TASK]  Run this task once and exit: 0 done, 1 not verified (a configured check did not pass on the final tree), 3 an error, 4 the model asked a question (answer it as the next task), 5 stopped short (2 is a bad argument)

Options:
      --resume         Resume the latest session in this repo
      --session <ID>   Resume this session of this repo (its id, as in [session ...])
  -m, --model <MODEL>  Model for a new session; with --resume it switches the session to it (so do /models and /model)
      --effort <LEVEL> How hard the model thinks: low, medium, high, xhigh or max, sent as reasoning_effort (DeepSeek runs
                       medium and xhigh as high); like -m it sets a new session and switches a resumed one (so do /effort and /models);
                       DeepSeek caches per level, so a switch sends the context uncached once
      --yolo           No sandbox: network on, writes anywhere (env vars stay filtered; env = [...] in ~/.config/low/config.toml passes more)
      --serve          After the task, keep the session and take the next tasks from low --send, keeping the provider's prompt cache warm in between (from a script, start it without a task)
      --send           Give the task to the low --serve running in this repo and print its result, with the same exit status; /stop ends that server
      --protect <PATH> Keep this file or folder from being changed in this run (repeatable; protect = [...] in low.toml does it always; with --yolo only edit() and write() refuse it)
      --tasks <FILE>   Work through the open tasks of a markdown checklist (- [ ] task) in one session, marking each line with its result; stops at the first task not done (one a turn limit cut off while it still changed files gets 2 more turns)
      --json           Show nothing while the task runs and print its result as one JSON object (with --send too)
  -h, --help           Print help
  -V, --version        Print version";

struct Cli {
    task: Option<String>,
    resume: bool,
    model: Option<String>,
    effort: Option<String>,
    yolo: bool,
    serve: bool,
    send: bool,
    json: bool,
    session: Option<String>,
    tasks: Option<PathBuf>,
    protect: Vec<PathBuf>,
}

impl Cli {
    /// Exits on --help and --version, and with status 2 on an argument low does not take.
    fn parse() -> Self {
        Self::args().map_err(|e| e.to_string()).and_then(Self::combined).unwrap_or_else(|e| {
            eprintln!("error: {e}\n\n{USAGE}\n\nFor more information, try '--help'.");
            std::process::exit(2)
        })
    }

    /// Options that don't go together.
    fn combined(self) -> Result<Self, String> {
        let send = !self.serve
            && !self.resume
            && self.model.is_none()
            && self.effort.is_none()
            && self.tasks.is_none()
            && !self.yolo
            && self.protect.is_empty();
        let refused = [
            (self.send && self.task.is_none(), "--send needs a task: low --send \"<task>\""),
            (
                self.effort.as_deref().is_some_and(|e| !agent::EFFORTS.contains(&e)),
                "--effort takes low, medium, high, xhigh or max",
            ),
            (
                self.send && !send,
                "--send takes only a task (and --session or --json); the server's session decides the rest",
            ),
            (
                self.json && self.task.is_none() && self.tasks.is_none(),
                "--json needs a task or --tasks; the REPL has no JSON",
            ),
            (self.json && self.serve, "--serve prints a log; use --json with --send instead"),
            (self.tasks.is_some() && (self.task.is_some() || self.serve), "--tasks takes its tasks from the file only"),
        ];
        match refused.into_iter().find(|(bad, _)| *bad) {
            Some((_, why)) => Err(why.into()),
            None => Ok(self),
        }
    }

    fn args() -> Result<Self, lexopt::Error> {
        use lexopt::prelude::*;
        let mut cli = Self {
            task: None,
            resume: false,
            model: None,
            effort: None,
            yolo: false,
            serve: false,
            send: false,
            json: false,
            session: None,
            tasks: None,
            protect: vec![],
        };
        let mut parser = lexopt::Parser::from_env();
        while let Some(arg) = parser.next()? {
            match arg {
                Value(task) if cli.task.is_none() => cli.task = Some(task.string()?),
                Long("resume") => cli.resume = true,
                Short('m') | Long("model") => cli.model = Some(parser.value()?.string()?),
                Long("effort") => cli.effort = Some(parser.value()?.string()?),
                Long("yolo") => cli.yolo = true,
                Long("serve") => cli.serve = true,
                Long("send") => cli.send = true,
                Long("json") => cli.json = true,
                Long("session") => cli.session = Some(parser.value()?.string()?),
                Long("tasks") => cli.tasks = Some(parser.value()?.into()),
                Long("protect") => cli.protect.push(parser.value()?.into()),
                Short('h') | Long("help") => {
                    println!("{ABOUT}\n\n{USAGE}\n\n{HELP}");
                    std::process::exit(0)
                }
                Short('V') | Long("version") => {
                    println!("low {VERSION}");
                    std::process::exit(0)
                }
                _ => return Err(arg.unexpected()),
            }
        }
        Ok(cli)
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub check: Option<String>,
    pub fmt: Option<String>,
    /// The provider whose API key search() and fetch() use (DeepSeek's tools API); a provider named deepseek if unset.
    pub search: Option<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
    /// Paths, relative to the repository, that the model's code and edits may not change (low.toml only).
    #[serde(default)]
    pub protect: Vec<String>,
    #[serde(skip)]
    pub providers: providers::Providers,
}

fn load(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| path.display().to_string()),
        Err(_) => Ok(Config::default()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // First of all: on macOS low starts over here without the secrets in its environment.
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let env = load(&providers::config_dir(&home).join("config.toml")).map(|c| c.env).unwrap_or_default();
    worker::scrub(&env);
    ui::guard();
    // reqwest brings no crypto of its own: every client, the agent's and /providers', uses ring.
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Err(e) = start().await {
        if std::env::args().any(|a| a == "--json") {
            println!("{}", agent::failed(None, &e));
        } else {
            eprintln!("error: {}", agent::error_text(&e));
        }
        std::process::exit(agent::ERROR as i32);
    }
}

async fn start() -> Result<()> {
    let cli = Cli::parse();
    // With --json, nothing low shows reaches stdout until the result: not the startup notes either.
    let quiet = (cli.json && !cli.send && !cli.serve).then(ui::Quiet::start).transpose()?;
    let home = PathBuf::from(std::env::var("HOME")?);
    ensure!(
        cli.task.as_deref().is_none_or(|t| !t.trim().is_empty()),
        "the task is empty: give low something to do, or run it without a task for the REPL"
    );
    let top = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output();
    let top = top.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let repo_dir = top.is_some();
    let root = top.map_or_else(std::env::current_dir, |t| Ok(PathBuf::from(t)))?.canonicalize()?;
    if cli.send {
        let task = cli.task.as_deref().context("--send needs a task: low --send \"<task>\"")?;
        let result = serve::send(&home, &root, cli.session.as_deref(), task, cli.json).await?;
        std::process::exit(exit_code(&result));
    }
    let user = load(&providers::config_dir(&home).join("config.toml"))?;
    check_config(&user)?;
    let repo = load(&root.join("low.toml"))?;
    let warn = |text: &str| {
        eprintln!("{text}");
        ui::record("note", text, serde_json::Value::Null);
    };
    if !repo_dir {
        warn(
            "not a git repository: low runs without undo here, so whatever it changes stays changed (git init adds undo)",
        );
    }
    if !cli.yolo && worker::read_only(&root) {
        warn(
            "this is your home directory or above it: low can read files here but not change them; cd into a folder to let it edit",
        );
    }
    if repo.model.is_some() || repo.base_url.is_some() || !repo.models.is_empty() {
        warn(
            "low.toml: model, base_url and models are ignored; models go in ~/.config/low/config.toml, providers in \
             ~/.config/low/providers.toml",
        );
    }
    if !repo.env.is_empty() {
        warn("low.toml: env is ignored; set env in ~/.config/low/config.toml");
    }
    let providers = providers::load(&home)?.unwrap_or_default();
    if let Some(name) = user.search.as_deref().filter(|name| !providers.contains_key(*name)) {
        bail!("~/.config/low/config.toml sets search = \"{name}\", but providers.toml has no [{name}]");
    }
    let search = user.search.clone().or_else(|| providers.contains_key("deepseek").then(|| "deepseek".into()));
    // A checklist names its session, so a later --tasks run continues it.
    let id = cli.session.clone().or_else(|| cli.tasks.as_deref().and_then(|file| tasks::session(&home, file)));
    let resume = cli.resume || id.is_some();
    let picked = cli.model.clone().filter(|_| resume);
    let config = Config {
        model: cli.model.or(user.model),
        base_url: None,
        check: repo.check.filter(|c| !c.trim().is_empty()),
        fmt: repo.fmt,
        search,
        env: user.env,
        models: user.models,
        protect: vec![],
        providers,
    };
    worker::allow_env(config.env.clone());
    // A --tasks checklist is low's to mark: the model reads it but doesn't change it. low.toml's paths are the
    // repository's; --protect and --tasks paths are the shell's, from wherever low runs.
    let cwd = std::env::current_dir()?;
    let given = cli.protect.iter().chain(&cli.tasks).map(|p| cwd.join(p));
    let protect = repo.protect.iter().map(|p| root.join(p)).chain(given);
    worker::protect(protect.map(|p| p.canonicalize().unwrap_or(p)).collect());
    let mut agent = agent::Agent::new(&root, &home, config, resume, id.as_deref(), cli.yolo, repo_dir)?;
    if let Some(model) = picked {
        agent.resume_with(&model)?;
    }
    if let Some(effort) = cli.effort.as_deref().filter(|e| agent.effort() != Some(*e)) {
        agent.set_effort(effort)?;
    }
    if !resume {
        eprintln!("[session {}]", agent.path().display());
    }
    if cli.serve {
        return serve::serve(&mut agent, cli.task).await;
    }
    if let Some(file) = &cli.tasks {
        let result = tasks::run(&mut agent, file).await?;
        drop((quiet, agent));
        if cli.json {
            println!("{result}");
        }
        std::process::exit(exit_code(&result));
    }
    if let Some(task) = cli.task {
        let result = agent.task(&task).await;
        drop((quiet, agent));
        match &result.error {
            _ if cli.json => println!("{result}"),
            Some(error) => eprintln!("error: {error}"),
            None => {}
        }
        std::process::exit(result.exit as i32);
    }
    let mut editor = rustyline::DefaultEditor::new()?;
    loop {
        let line = match editor.readline("› ") {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => continue,
            Err(_) => return Ok(()),
        };
        let line = line.trim();
        if let Some(why) = unknown(line) {
            let _ = editor.add_history_entry(line);
            eprintln!("error: {why}");
            continue;
        }
        let result = match line {
            "" => Ok(()),
            "/compact" => agent.compact(),
            "/cost" => {
                agent.cost();
                Ok(())
            }
            "/think" => {
                agent.think();
                Ok(())
            }
            "/providers" => {
                async {
                    let mut providers = providers::load(&home)?.unwrap_or_default();
                    let result = providers::menu(&home, &mut providers, agent.provider(), &mut editor).await;
                    agent.set_providers(providers).and(result)
                }
                .await
            }
            _ if let Some(filter) = argument(line, "/models").or_else(|| argument(line, "/model")) => {
                models(&mut agent, &mut editor, filter).await
            }
            _ if let Some(level) = argument(line, "/effort") => effort(&mut agent, level),
            task => {
                editor.add_history_entry(task)?;
                agent.run(task).await.map(drop)
            }
        };
        let error = result.err().map(|e| agent::error_text(&e));
        if COMMANDS.contains(&line.split_whitespace().next().unwrap_or_default()) {
            ui::record("command", line, serde_json::json!({"error": error}));
        }
        if let Some(error) = error {
            eprintln!("error: {error}");
        }
    }
}

/// What follows a command's name on its line, if the line is that command.
fn argument<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.strip_prefix(name).filter(|rest| rest.is_empty() || rest.starts_with(' ')).map(str::trim)
}

/// /models and /model: on a terminal, one panel for the model and the reasoning effort; elsewhere a numbered list.
async fn models(agent: &mut agent::Agent, editor: &mut rustyline::DefaultEditor, filter: &str) -> Result<()> {
    use std::io::IsTerminal;
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !terminal || ui::size().is_none() || std::env::var_os("TERM").is_some_and(|t| t == "dumb") {
        return pick(agent, editor, filter).await;
    }
    let now = (agent.provider().to_string(), agent.model().to_string());
    let mut rows: Vec<picker::Row> = agent
        .models()
        .await
        .into_iter()
        .map(|(provider, model, profiled)| {
            let current = (&provider, &model) == (&now.0, &now.1);
            picker::Row { provider, model, profiled, current }
        })
        .collect();
    // The session's model is a row even when no provider lists it now.
    if !rows.iter().any(|r| r.current) {
        let profiled = agent.profile(&now.1).is_some();
        rows.insert(0, picker::Row { provider: now.0.clone(), model: now.1.clone(), profiled, current: true });
    }
    let default = |agent: &agent::Agent, model: &str| -> Option<String> {
        agent.profile(model)?.get("reasoning_effort")?.as_str().map(String::from)
    };
    let defaults = rows.iter().filter_map(|r| Some((r.model.clone(), default(agent, &r.model)?))).collect();
    // The session's effort is picked as the default when it is just what the profile sets.
    let explicit = agent.effort().filter(|e| default(agent, &now.1).as_deref() != Some(*e));
    let mut state = picker::State::new(rows, defaults, filter, explicit);
    let Some(picked) = picker::choose(&mut state)? else { return Ok(()) };
    let rows = state.rows();
    let (provider, model) = match picked.target {
        picker::Target::Listed(i) => (rows[i].provider.clone(), rows[i].model.clone()),
        picker::Target::Typed(typed) => {
            let (provider, model) = agent.resolve(&typed);
            ui::note(&format!("{} is not listed; trying it on {provider}", ui::printable(&model)));
            (provider, model)
        }
    };
    let profile = default(agent, &model);
    let to = (provider.as_str(), model.as_str());
    for step in picker::steps((&now.0, &now.1), agent.effort(), to, profile.as_deref(), picked.effort) {
        match step {
            picker::Step::Switch => agent.switch(&provider, &model)?,
            picker::Step::Effort(level) => agent.set_effort(level)?,
        }
    }
    Ok(())
}

async fn pick(agent: &mut agent::Agent, editor: &mut rustyline::DefaultEditor, filter: &str) -> Result<()> {
    let needle = filter.to_lowercase();
    let all = agent.models().await;
    let models: Vec<&(String, String, bool)> =
        all.iter().filter(|(_, m, _)| m.to_lowercase().contains(&needle)).collect();
    for (i, (provider, model, profiled)) in (1..).zip(models.iter().copied()) {
        let current = provider == agent.provider() && model == agent.model();
        let current = if current { ui::paint("32", " ● current") } else { String::new() };
        let profile = if *profiled { ui::paint("2", " (profile)") } else { String::new() };
        let on = ui::paint("2", &format!("  {provider}"));
        let model = ui::printable(model);
        println!("{} {model}{on}{profile}{current}", ui::paint("2", &format!("{i:>3}")));
    }
    let choice = match editor.readline("model number or name (enter keeps the current one) › ") {
        Ok(choice) => choice.trim().to_string(),
        Err(_) => return Ok(()),
    };
    if choice.is_empty() {
        return Ok(());
    }
    let listed = match choice.parse::<usize>() {
        Ok(n) => Some(*models.get(n.wrapping_sub(1)).with_context(|| format!("no model numbered {n}"))?),
        Err(_) => all
            .iter()
            .find(|(p, m, _)| (p == agent.provider() && *m == choice) || format!("{p}:{m}") == choice)
            .or_else(|| all.iter().find(|(_, m, _)| *m == choice)),
    };
    let (provider, model) = match listed {
        Some((provider, model, _)) => (provider.clone(), model.clone()),
        None => {
            ui::note(&format!("{} is not listed; trying it on {}", ui::printable(&choice), agent.provider()));
            (agent.provider().to_string(), choice)
        }
    };
    if provider == agent.provider() && model == agent.model() {
        return Ok(());
    }
    agent.switch(&provider, &model)
}

/// The exit status a --tasks or --send result gives (agent::outcome), an error's when it has none.
fn exit_code(result: &serde_json::Value) -> i32 {
    result["exit"].as_i64().unwrap_or(agent::ERROR) as i32
}

/// /effort alone shows the reasoning effort the session's requests carry and the choices; /effort <level> sets it.
fn effort(agent: &mut agent::Agent, level: &str) -> Result<()> {
    if level.is_empty() {
        let now = agent.effort().unwrap_or("not set, so the provider's default");
        let choices = agent::EFFORTS.join(" ");
        println!(
            "reasoning effort: {now}\nset it with /effort {choices}, or pick it in /models (DeepSeek runs medium and xhigh as high)"
        );
        return Ok(());
    }
    agent.set_effort(level)
}

const COMMANDS: &[&str] = &["/compact", "/cost", "/think", "/effort", "/model", "/models", "/providers"];

/// A "/word" line that names no command is a typo, and must not reach the model as a task.
fn unknown(line: &str) -> Option<String> {
    let word = line.split_whitespace().next()?;
    let name = word.strip_prefix('/')?;
    if name.contains('/') || COMMANDS.contains(&word) {
        return None;
    }
    let near = COMMANDS.iter().copied().find(|c| !name.is_empty() && (c.starts_with(word) || word.starts_with(c)));
    Some(match near {
        Some(near) => format!("unknown command {word}; did you mean {near}?"),
        None => format!("unknown command {word}; the commands are {}", COMMANDS.join(" ")),
    })
}

/// Stops on a setting that belongs in a repository's low.toml or that low no longer reads, or on a profile value it
/// cannot use.
fn check_config(user: &Config) -> Result<()> {
    if user.check.is_some() || user.fmt.is_some() {
        bail!("~/.config/low/config.toml sets check or fmt; they belong to one repository, so they go in its low.toml");
    }
    if !user.protect.is_empty() {
        bail!(
            "~/.config/low/config.toml sets protect; it names paths of one repository, so it goes in that repository's low.toml"
        );
    }
    if user.base_url.is_some() {
        let model = user.model.as_deref().unwrap_or("model-id");
        bail!("~/.config/low/config.toml sets base_url, which low no longer reads; {}", providers::example(model));
    }
    for (model, profile) in &user.models {
        let at = format!("~/.config/low/config.toml: [models.\"{model}\"]");
        if let Some(key) = ["think_limit", "turn_tokens", "guidance"].into_iter().find(|k| profile.contains_key(*k)) {
            bail!("{at} sets {key}, which low no longer supports; remove it");
        }
        if let Some(value) = profile.get("compact_at").filter(|v| v.as_u64().is_none_or(|n| n < 1024)) {
            bail!("{at} sets compact_at = {value}; it takes a whole number of tokens, at least 1024");
        }
        if let Some(value) = profile.get("keep_warm").filter(|v| !v.is_u64()) {
            bail!("{at} sets keep_warm = {value}; it takes a whole number of seconds, 0 to turn it off");
        }
        if let Some(value) = profile.get("fallback").filter(|v| v.as_str().is_none_or(|f| !f.contains(':'))) {
            bail!("{at} sets fallback = {value}; it takes \"provider:model\", e.g. \"opencode:deepseek-v4.1-flash\"");
        }
        for key in ["turn_budget", "session_budget"] {
            if let Some(value) = profile.get(key).filter(|v| v.as_u64().is_none_or(|n| n < 1)) {
                bail!("{at} sets {key} = {value}; it takes a whole number of compact_at units, at least 1");
            }
        }
    }
    Ok(())
}
