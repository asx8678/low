//! A session is one JSONL file in ~/.air/sessions, named <unix secs>-<pid>. Line 0 is the header: settings, cwd,
//! build, git and sandbox. Each later line is one of
//! - a message {role, content, tool_calls?, reasoning_content?, meta}: an assistant's meta holds the request's
//!   usage, timing, request and response; a tool's meta holds the run's actions, commands, errors and ms
//! - {event, text, ...}: retries, cut streams, nudges, the notes shown to the user, connects, commands
//! - {check}, {report} per turn, {compact}, {switch}, {resume}, and {end} with this run's totals
//!
//! Every line has a wall-clock `at` in ms, in meta for messages since meta never goes on the wire. Nothing secret
//! goes into it.
use anyhow::{Result, bail};
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
}

#[derive(Default)]
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
        let dir = home.join(".air/sessions");
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let path = dir.join(format!("{secs}-{}.jsonl", std::process::id()));
        let file = OpenOptions::new().create_new(true).append(true).mode(0o600).open(&path)?;
        file.try_lock()?;
        let mut session = Self { path, entries: vec![], file, opened: (0, Instant::now()) };
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
        self.entries.iter().rev().find_map(|e| e.get("switch")).unwrap_or(&self.entries[0])
    }

    pub fn resume(home: &Path, root: &Path, context: Map<String, Value>) -> Result<Self> {
        let mut paths: Vec<PathBuf> = fs::read_dir(home.join(".air/sessions"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        paths.sort();
        for path in paths.into_iter().rev() {
            let Ok(Some(Ok(first))) = File::open(&path).map(|f| BufReader::new(f).lines().next()) else { continue };
            if serde_json::from_str::<Value>(&first).is_ok_and(|header| header["cwd"] == json!(root)) {
                let mut file = OpenOptions::new().append(true).open(&path)?;
                if file.try_lock().is_err() {
                    bail!("{} is in use by another air", path.display());
                }
                let mut text = fs::read_to_string(&path)?;
                let cut = text.rfind('\n').map_or(0, |i| i + 1);
                if serde_json::from_str::<Value>(&text[cut..]).is_ok() {
                    file.write_all(b"\n")?;
                    text.push('\n');
                } else {
                    file.set_len(cut as u64)?;
                    text.truncate(cut);
                }
                let mut entries: Vec<Value> = text.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
                backfill_id(&mut entries, &path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default());
                eprintln!("[resumed {}]", path.display());
                let opened = (entries.len(), Instant::now());
                let mut session = Self { file, path, entries, opened };
                session.push(json!({"resume": context}))?;
                return Ok(session);
            }
        }
        bail!("no session to resume for {}", root.display())
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
        self.entries.push(entry);
        Ok(())
    }

    /// The private git index change detection uses: a cache for this run, rebuilt from the repo's index when missing.
    pub fn index(&self) -> PathBuf {
        self.path.with_extension("index")
    }

    pub fn keep(&self) -> usize {
        self.entries.iter().rev().find_map(|e| e["compact"]["keep"].as_u64()).map_or(1, |k| k as usize)
    }

    /// The prompt size the provider last reported; 0 once a compaction since then has made it stale.
    pub fn last_prompt_tokens(&self) -> u64 {
        let current = self.entries.iter().rev().take_while(|e| e["compact"].is_null());
        current.map(|e| &e["meta"]["usage"]["prompt_tokens"]).find_map(Value::as_u64).unwrap_or(0)
    }

    pub fn messages(&self) -> Vec<Value> {
        let mut out = vec![json!({"role": "system", "content": self.header()["system"]})];
        let (keep, newest) = (self.keep(), crate::compact::newest_task(&self.entries));
        if let Some(summary) = self.entries.iter().rev().find_map(|e| e["compact"]["summary"].as_str()) {
            let mut content = summary.to_string();
            if let Some(u) = newest.filter(|&u| u < keep && !summary.contains(&crate::compact::baked(u))) {
                let task = self.entries[u]["content"].as_str().unwrap_or_default();
                content += &format!("\n\n[Current task, entry #{u}]\n{task}");
            }
            out.push(json!({"role": "user", "content": content}));
        }
        let mut open: Vec<Value> = vec![];
        for (i, entry) in self.entries.iter().enumerate().skip(keep) {
            let Some(role) = entry["role"].as_str() else { continue };
            let message = crate::compact::wire(entry, i, newest);
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
                let label = if entry["meta"]["host"].is_null() {
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
        let mut state = State::default();
        for entry in &self.entries {
            if entry["role"] == "user" && entry["meta"]["host"].is_null() {
                state.accepted = None;
                state.plan = Value::Null;
            }
            let meta = &entry["meta"];
            if let Some(criteria) = meta["accepted"].as_str() {
                state.accepted = Some(criteria.into());
            }
            if meta["plan"].is_object() {
                state.plan = meta["plan"].clone();
            }
        }
        state
    }
}

/// Every way out of air short of a signal drops the session, so each run ends with its totals.
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.flush();
        let (from, clock) = self.opened;
        let mut totals = crate::stats::totals(&self.entries[from..]);
        totals["ms"] = (clock.elapsed().as_millis() as u64).into();
        let _ = self.push(json!({"end": totals}));
        // One per session would pile up in ~/.air/sessions; a resumed session builds a fresh one.
        let _ = std::fs::remove_file(self.index());
    }
}

fn stamp(entry: &mut Value, at: u64) {
    let fields = if entry["role"].is_string() { &mut entry["meta"] } else { entry };
    if fields.get("at").is_none() {
        fields["at"] = at.into();
    }
}

fn settings(provider: &str, model: &str, system: &str, mut params: Map<String, Value>) -> Value {
    let compact_at = params.remove("compact_at");
    params.entry("temperature").or_insert(crate::llm::TEMPERATURE.into());
    params.entry("reasoning_effort").or_insert("high".into());  // experiment: pinned reasoning setting
    json!({"provider": provider, "model": model, "params": params, "compact_at": compact_at, "system": system})
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
    use super::{backfill_id, stamp};
    use serde_json::json;

    #[test]
    fn stamps_stay_off_the_wire() {
        let (mut user, mut event) = (json!({"role": "user", "content": "x"}), json!({"event": "retry", "at": 1}));
        stamp(&mut user, 7);
        stamp(&mut event, 7);
        assert_eq!(user, json!({"role": "user", "content": "x", "meta": {"at": 7}}));
        assert_eq!(event["at"], 1);
        assert_eq!(crate::compact::wire(&user, 1, None), json!({"role": "user", "content": "x"}));
    }

    #[test]
    fn resumed_logs_get_the_file_id() {
        let mut entries = vec![
            json!({"model": "m"}),
            json!({"role": "user"}),
            json!({"switch": {"model": "n"}}),
            json!({"switch": {"id": "kept"}}),
        ];
        backfill_id(&mut entries, "1792-42");
        assert_eq!(entries[0]["id"], "1792-42");
        assert_eq!(entries[1].get("id"), None);
        assert_eq!(entries[2]["switch"]["id"], "1792-42");
        assert_eq!(entries[3]["switch"]["id"], "kept");
    }
}
