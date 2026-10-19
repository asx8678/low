//! A session is one JSONL file in ~/.low/sessions, named <unix secs>-<pid>. Line 0 is the header: settings, cwd,
//! build, git and sandbox. Each later line is one of
//! - a message {role, content, tool_calls?, reasoning_content?, meta}: an assistant's meta holds the request's
//!   usage, timing, request and response; a tool's meta holds the run's actions, commands, errors and ms
//! - {event, text, ...}: retries, cut streams, nudges, cache misses, the notes shown to the user, connects, commands
//! - {check}, {report} per turn, {compact}, {switch}, {resume}, and {end} with this run's totals
//!
//! Every line has a wall-clock `at` in ms, in meta for messages since meta never goes on the wire. Nothing secret
//! goes into it.
use anyhow::{Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct Session {
    pub path: PathBuf,
    pub entries: Vec<Value>,
    file: File,
    opened: (usize, Instant),
    marks: Marks,
}

/// What every request looks up in the log, kept up as entries arrive, so that no request walks a log that only grows
/// (a --serve session's runs to 100k entries).
#[derive(Default)]
struct Marks {
    /// The last {switch}, or 0 for the header.
    header: usize,
    /// The last {compact}, and the last that names its keep and its summary.
    compact: Option<usize>,
    keep: Option<usize>,
    summary: Option<usize>,
    /// The newest task: a user message that is not from low and does not steer a turn.
    task: Option<usize>,
    state: State,
    /// What the replies have spent, weighed as a turn's budget weighs them.
    spent: u64,
    /// The folders' instructions (AGENTS.md) runs have shown, each with the entry of the run that showed it.
    guides: Vec<(usize, String)>,
}

impl Marks {
    fn add(&mut self, i: usize, entry: &Value) {
        if entry.get("switch").is_some() {
            self.header = i;
        }
        let compact = &entry["compact"];
        if compact.is_object() {
            self.compact = Some(i);
        }
        if let Some(keep) = compact["keep"].as_u64() {
            self.keep = Some(keep as usize);
        }
        if compact["summary"].is_string() {
            self.summary = Some(i);
        }
        if i > 0 && crate::compact::task(entry) {
            self.task = Some(i);
            self.state = State::default();
        }
        let meta = &entry["meta"];
        if let Some(criteria) = meta["accepted"].as_str() {
            self.state.accepted = Some(criteria.into());
        }
        if meta["plan"].is_object() {
            self.state.plan = meta["plan"].clone();
        }
        if entry["role"] == "assistant" || meta["aside"] == true {
            self.spent += crate::agent::weighted(meta);
        }
        if entry["role"] == "tool" {
            let shown = meta["guides"].as_array().into_iter().flatten().filter_map(Value::as_str);
            self.guides.extend(shown.map(|g| (i, g.to_string())));
        }
    }
}

#[derive(Default, Clone)]
pub struct State {
    pub accepted: Option<String>,
    pub plan: Value,
}

impl Session {
    pub fn create(
        home: &Path,
        root: &Path,
        provider: &str,
        model: &str,
        system: &str,
        profile: Map<String, Value>,
        context: Map<String, Value>,
    ) -> Result<Self> {
        let dir = home.join(".low/sessions");
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        sweep(&dir);
        let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let path = dir.join(format!("{secs}-{}.jsonl", std::process::id()));
        let file = OpenOptions::new().create_new(true).append(true).mode(0o600).open(&path)?;
        file.try_lock()?;
        let mut session = Self { path, entries: vec![], file, opened: (0, Instant::now()), marks: Marks::default() };
        let mut header = settings(provider, model, system, profile);
        header["cwd"] = json!(root);
        // One stable id per conversation; OpenCode's gateway routes by it.
        header["id"] = session.path.file_stem().map(|s| s.to_string_lossy().into_owned()).into();
        if let Some(fields) = header.as_object_mut() {
            fields.extend(context);
        }
        // Not push(): events noted before the session existed must not land above the header.
        session.write(header)?;
        Ok(session)
    }

    pub fn switch(&mut self, provider: &str, model: &str, system: &str, profile: Map<String, Value>) -> Result<()> {
        let mut switched = settings(provider, model, system, profile);
        if let Some(id) = self.header()["id"].as_str() {
            switched["id"] = id.into();
        }
        self.push(json!({"switch": switched, "at": crate::ui::now_ms()}))
    }

    pub fn header(&self) -> &Value {
        let at = self.marks.header;
        self.entries[at].get("switch").unwrap_or(&self.entries[at])
    }

    /// The newest session of this repository, or the one `id` names (its id, or its log's path).
    pub fn resume(home: &Path, root: &Path, context: Map<String, Value>, id: Option<&str>) -> Result<Self> {
        let id = id.map(|id| Path::new(id).file_stem().and_then(|s| s.to_str()).unwrap_or(id));
        sweep(&home.join(".low/sessions"));
        let mut paths: Vec<PathBuf> = fs::read_dir(home.join(".low/sessions"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .filter(|p| id.is_none_or(|id| p.file_stem().is_some_and(|s| s == id)))
            .collect();
        paths.sort();
        for path in paths.into_iter().rev() {
            let Ok(Some(Ok(first))) = File::open(&path).map(|f| BufReader::new(f).lines().next()) else { continue };
            if serde_json::from_str::<Value>(&first).is_ok_and(|header| header["cwd"] == json!(root)) {
                let mut file = OpenOptions::new().append(true).open(&path)?;
                ensure!(file.try_lock().is_ok(), "{} is in use by another low", path.display());
                let mut text = fs::read_to_string(&path)?;
                let cut = text.rfind('\n').map_or(0, |i| i + 1);
                if serde_json::from_str::<Value>(&text[cut..]).is_ok() {
                    file.write_all(b"\n")?;
                    text.push('\n');
                } else {
                    // A crash while writing left part of a line (a log that ends in its newline has nothing after it).
                    let torn = text.len() - cut;
                    if torn > 0 {
                        let what = format!("dropped an incomplete last line ({torn} bytes) from {}", path.display());
                        crate::ui::note(&what);
                    }
                    file.set_len(cut as u64)?;
                    text.truncate(cut);
                }
                let mut entries: Vec<Value> = text.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
                backfill_id(&mut entries, &path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default());
                eprintln!("[resumed {}]", path.display());
                let opened = (entries.len(), Instant::now());
                let mut marks = Marks::default();
                entries.iter().enumerate().for_each(|(i, entry)| marks.add(i, entry));
                let mut session = Self { file, path, entries, opened, marks };
                session.push(json!({"resume": context}))?;
                return Ok(session);
            }
        }
        match id {
            Some(id) => bail!("no session {id} for {}", root.display()),
            None => bail!("no session to resume for {}", root.display()),
        }
    }

    pub fn push(&mut self, entry: Value) -> Result<()> {
        self.flush()?;
        self.write(entry)
    }

    pub fn flush(&mut self) -> Result<()> {
        crate::ui::events().into_iter().try_for_each(|event| self.write(event))
    }

    fn write(&mut self, mut entry: Value) -> Result<()> {
        stamp(&mut entry, crate::ui::now_ms());
        self.file.write_all(format!("{entry}\n").as_bytes())?;
        self.marks.add(self.entries.len(), &entry);
        self.entries.push(entry);
        Ok(())
    }

    /// The private git index change detection uses: a cache for this run, rebuilt from the repo's index when missing.
    pub fn index(&self) -> PathBuf {
        self.path.with_extension("index")
    }

    pub fn keep(&self) -> usize {
        self.marks.keep.unwrap_or(1)
    }

    /// The folders' instructions the context still holds: those shown by runs from the kept tail on. One that went
    /// into a compaction is shown again the next time a run touches its folder.
    pub fn guides(&self) -> Vec<&str> {
        let keep = self.keep();
        self.marks.guides.iter().filter(|(i, _)| *i >= keep).map(|(_, g)| g.as_str()).collect()
    }

    /// The last {compact} entry.
    pub fn compaction(&self) -> Option<usize> {
        self.marks.compact
    }

    /// The newest task's entry: a user message that is not from low and does not steer a turn.
    pub fn newest_task(&self) -> Option<usize> {
        self.marks.task
    }

    /// What the session's replies have spent, weighed as a turn's budget weighs them.
    pub fn spent(&self) -> u64 {
        self.marks.spent
    }

    /// The prompt size the provider last reported; 0 once a compaction since then has made it stale.
    pub fn last_prompt_tokens(&self) -> u64 {
        let current = self.entries.iter().rev().take_while(|e| e["compact"].is_null());
        current.filter_map(prompt_tokens).next().unwrap_or(0)
    }

    pub fn messages(&self) -> Vec<Value> {
        let mut out = vec![json!({"role": "system", "content": self.header()["system"]})];
        let (keep, newest) = (self.keep(), self.newest_task());
        if let Some(summary) = self.marks.summary.and_then(|at| self.entries[at]["compact"]["summary"].as_str()) {
            let mut content = summary.to_string();
            if let Some(u) = newest.filter(|&u| u < keep && !summary.contains(&crate::compact::baked(u))) {
                let task = crate::compact::wire(&self.entries[u]);
                let task = task["content"].as_str().unwrap_or_default();
                let steers = crate::compact::steers(&self.entries, u, keep);
                content += &format!("\n\n[Current task, entry #{u}]\n{task}{steers}");
            }
            out.push(json!({"role": "user", "content": content}));
        }
        let mut open: Vec<Value> = vec![];
        for (i, entry) in self.entries.iter().enumerate().skip(keep) {
            let Some(role) = entry["role"].as_str() else { continue };
            let message = crate::compact::wire(entry);
            if role == "tool" {
                if let Some(i) = open.iter().position(|id| *id == message["tool_call_id"]) {
                    open.remove(i);
                    out.push(message);
                }
                continue;
            }
            answer_missing(&mut out, &mut open);
            if role == "user"
                && let Some(prev) = out.last_mut().filter(|m| m["role"] == "user")
            {
                let label = if entry["meta"]["steer"] == true {
                    format!("[Steered by the user, entry #{i}]\n")
                } else if entry["meta"]["host"].is_null() {
                    format!("[Next user message, entry #{i}]\n")
                } else {
                    String::new()
                };
                let joined = format!(
                    "{}\n\n{label}{}",
                    prev["content"].as_str().unwrap_or_default(),
                    message["content"].as_str().unwrap_or_default()
                );
                prev["content"] = joined.into();
                continue;
            }
            if role == "assistant" {
                open = message["tool_calls"]
                    .as_array()
                    .map(|c| c.iter().map(|c| c["id"].clone()).collect())
                    .unwrap_or_default();
            }
            out.push(message);
        }
        answer_missing(&mut out, &mut open);
        out
    }

    pub fn state(&self) -> State {
        self.marks.state.clone()
    }
}

/// Every way out of low short of a signal drops the session, so each run ends with its totals.
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.flush();
        let (from, clock) = self.opened;
        let mut totals = crate::stats::totals(&self.entries[from..]);
        totals["ms"] = crate::ui::ms(clock).into();
        let _ = self.push(json!({"end": totals}));
        // One per session would pile up in ~/.low/sessions; a resumed session builds a fresh one.
        let _ = std::fs::remove_file(self.index());
    }
}

/// Removes the change-detection indexes no running low holds: a low that was killed leaves its own behind. A session's
/// log stays locked while a low has it open, a --serve included, however long it has been idle.
fn sweep(dir: &Path) {
    let indexes = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path());
    for index in indexes.filter(|p| p.extension().is_some_and(|e| e == "index")) {
        if !crate::serve::locked(&index.with_extension("jsonl")) {
            let _ = fs::remove_file(index);
        }
    }
}

/// The prompt size a request of the conversation reported. The second look's requests (meta.aside) are a conversation of
/// their own, so they count in the totals but not here.
pub fn prompt_tokens(entry: &Value) -> Option<u64> {
    entry["meta"]["usage"]["prompt_tokens"].as_u64().filter(|_| entry["meta"]["aside"] != true)
}

fn stamp(entry: &mut Value, at: u64) {
    let fields = if entry["role"].is_string() { &mut entry["meta"] } else { entry };
    if fields.get("at").is_none() {
        fields["at"] = at.into();
    }
}

fn settings(provider: &str, model: &str, system: &str, mut params: Map<String, Value>) -> Value {
    let (compact_at, keep_warm) = (params.remove("compact_at"), params.remove("keep_warm"));
    let (turn_budget, fallback) = (params.remove("turn_budget"), params.remove("fallback"));
    let session_budget = params.remove("session_budget");
    // temperature = false in a model's profile sends none, for models that refuse the field.
    if params.get("temperature") == Some(&Value::Bool(false)) {
        params.remove("temperature");
    } else {
        params.entry("temperature").or_insert(crate::llm::TEMPERATURE.into());
    }
    json!({"provider": provider, "model": model, "params": params, "compact_at": compact_at, "keep_warm": keep_warm,
           "turn_budget": turn_budget, "session_budget": session_budget, "fallback": fallback, "system": system})
}

fn answer_missing(out: &mut Vec<Value>, open: &mut Vec<Value>) {
    for id in open.drain(..) {
        out.push(json!({"role": "tool", "tool_call_id": id, "content": "tool result missing (interrupted)"}));
    }
}

/// Logs written before the conversation id existed get the id create() would have given them, so resuming one still
/// sends x-opencode-session.
fn backfill_id(entries: &mut [Value], id: &str) {
    for (i, entry) in entries.iter_mut().enumerate() {
        let header =
            if i == 0 { entry.as_object_mut() } else { entry.get_mut("switch").and_then(Value::as_object_mut) };
        if let Some(header) = header.filter(|h| h.get("id").and_then(Value::as_str).is_none_or(str::is_empty)) {
            header.insert("id".into(), id.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Session;
    use serde_json::{Map, Value, json};

    fn assistant(content: &str, calls: &[&str], prompt: u64) -> Value {
        let calls: Vec<Value> = calls
            .iter()
            .map(|id| json!({"id": id, "type": "function", "function": {"name": "run", "arguments": "{}"}}))
            .collect();
        let mut entry = json!({"role": "assistant", "content": content,
                               "meta": {"usage": {"prompt_tokens": prompt, "completion_tokens": 10}}});
        if !calls.is_empty() {
            entry["tool_calls"] = calls.into();
        }
        entry
    }

    /// What a request is built from, live and after a resume: the same messages, header, state, keep and spend.
    #[test]
    fn messages_and_marks_survive_resume() {
        let home = std::env::temp_dir().join(format!("low-session-test-{}", std::process::id()));
        let root = home.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let mut session = Session::create(&home, &root, "p", "m", "sys", Map::new(), Map::new()).unwrap();
        let entries = [
            json!({"role": "user", "content": "first task", "meta": {}}),
            assistant("", &["a"], 1000),
            json!({"role": "tool", "tool_call_id": "a", "content": "ok",
                   "meta": {"accepted": "it works", "plan": {"steps": ["x"]}, "guides": ["pkg/AGENTS.md"]}}),
            json!({"role": "user", "content": "keep going", "meta": {"host": true}}),
            assistant("", &["b", "c"], 2000),
            json!({"role": "tool", "tool_call_id": "c", "content": "ran c", "meta": {"guides": ["pkg/sub/AGENTS.md"]}}),
            json!({"role": "user", "content": "second task", "meta": {}}),
        ];
        for entry in entries {
            session.push(entry).unwrap();
        }
        let messages = session.messages();
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "user", "assistant", "tool", "tool", "user"]);
        // A call that never got its result is answered, in the order the reply made them.
        assert_eq!((&messages[6]["tool_call_id"], &messages[6]["content"]), (&json!("c"), &json!("ran c")));
        assert_eq!(messages[7]["content"], "tool result missing (interrupted)");
        assert!(messages.iter().all(|m| m.get("meta").is_none()));
        // A new task drops the last one's criteria and plan; a steer keeps them.
        assert_eq!((session.state().accepted, session.newest_task()), (None, Some(7)));
        session
            .push(json!({"role": "tool", "tool_call_id": "z", "content": "", "meta": {"accepted": "done"}}))
            .unwrap();
        session.push(json!({"role": "user", "content": "use tabs", "meta": {"steer": true}})).unwrap();
        assert_eq!((session.state().accepted.as_deref(), session.newest_task()), (Some("done"), Some(7)));
        assert_eq!(session.spent(), 3020);
        // The folders' instructions runs showed stay known across tasks, until a compaction drops their runs.
        assert_eq!(session.guides(), ["pkg/AGENTS.md", "pkg/sub/AGENTS.md"]);
        session.switch("p", "m2", "sys2", Map::new()).unwrap();
        session.push(json!({"compact": {"keep": 10, "summary": "SUMMARY"}})).unwrap();
        session.push(json!({"role": "user", "content": "more", "meta": {"steer": true}})).unwrap();
        let messages = session.messages();
        assert_eq!((&messages[0]["content"], &messages[1]["role"]), (&json!("sys2"), &json!("user")));
        let joined = messages[1]["content"].as_str().unwrap();
        assert!(joined.starts_with("SUMMARY") && joined.contains("[Steered by the user, entry #12]\nmore"), "{joined}");
        // The task and its steers before the kept tail follow a summary written before them.
        assert!(joined.contains("[Current task, entry #7]\nsecond task\n\n[Steered by the user, entry #9]\nuse tabs"));
        assert!(session.guides().is_empty());
        session
            .push(json!({"role": "tool", "tool_call_id": "y", "content": "", "meta": {"guides": ["lib/AGENTS.md"]}}))
            .unwrap();
        assert_eq!(session.guides(), ["lib/AGENTS.md"]);
        let messages = session.messages();
        let live = (messages, session.header().clone(), session.keep(), session.compaction(), session.spent());
        drop(session);
        // A process another test forks meanwhile holds the log's lock until it execs.
        let mut resumed = Session::resume(&home, &root, Map::new(), None);
        for _ in 0..50 {
            if resumed.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            resumed = Session::resume(&home, &root, Map::new(), None);
        }
        let session = resumed.unwrap();
        let resumed =
            (session.messages(), session.header().clone(), session.keep(), session.compaction(), session.spent());
        assert_eq!(live, resumed);
        assert_eq!(session.guides(), ["lib/AGENTS.md"]);
        assert_eq!((resumed.1["model"].as_str(), resumed.2, resumed.3), (Some("m2"), 10, Some(11)));
        drop(session);
        let _ = std::fs::remove_dir_all(&home);
    }
}
