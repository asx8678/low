//! `low --serve` keeps one session open between tasks and takes each next task from `low --send`. A client leaves its
//! task as a file in an inbox folder next to the session log and waits for the result file (a Unix socket there could
//! pass the 104-byte path limit). The server holds the session's lock while it runs, which is how a client tells a
//! live server from one that died. While it waits, the server keeps the provider's prompt cache warm (Agent::warm).
use crate::agent::{self, Agent};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs::{self, DirBuilder, File};
use std::io::BufRead;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the server looks for a task, and a client for its result.
const POLL: Duration = Duration::from_millis(250);

/// The inbox folder, removed when the server stops (one that is killed leaves it; the next client clears it).
struct Inbox(PathBuf);

impl Drop for Inbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub async fn serve(agent: &mut Agent, first: Option<String>) -> Result<()> {
    let dir = agent.path().with_extension("inbox");
    // This process holds the session's lock, so an inbox already there was left by a server that died.
    let _ = fs::remove_dir_all(&dir);
    DirBuilder::new().mode(0o700).create(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    let _inbox = Inbox(dir.clone());
    eprintln!("[serving: send tasks with low --send \"<task>\" in this repository; low --send /stop ends it]");
    if let Some(task) = first
        && let Err(e) = agent.run(&task).await
    {
        eprintln!("error: {}", agent::error_text(&e));
    }
    let every = agent.keep_warm();
    let mut idle = tokio::time::Instant::now();
    let mut poll = tokio::time::interval(POLL);
    loop {
        tokio::select! {
            _ = poll.tick() => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        if let Some((name, task)) = next(&dir)? {
            let reply = answer(agent, &task).await;
            let (temp, done) = (dir.join(format!("{name}.part")), dir.join(format!("{name}.result")));
            // Written whole, then renamed, so a client never reads half a result.
            fs::write(&temp, format!("{reply}\n")).and_then(|()| fs::rename(&temp, &done))?;
            if reply["stopped"] == true {
                // The inbox goes with the server: give the client a moment to read the result first.
                for _ in 0..10 {
                    if !done.exists() {
                        break;
                    }
                    tokio::time::sleep(POLL).await;
                }
                return Ok(());
            }
            idle = tokio::time::Instant::now();
        } else if every.is_some_and(|every| idle.elapsed() >= every) {
            if let Err(e) = agent.warm().await {
                crate::ui::note(&format!("could not keep the prompt cache warm: {}", agent::error_text(&e)));
            }
            idle = tokio::time::Instant::now();
        }
    }
}

/// The oldest task waiting in the inbox, taken out of it.
fn next(dir: &Path) -> Result<Option<(String, String)>> {
    let mut tasks: Vec<PathBuf> =
        fs::read_dir(dir)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "task")).collect();
    tasks.sort();
    let Some(path) = tasks.into_iter().next() else { return Ok(None) };
    let text = fs::read_to_string(&path)?;
    fs::remove_file(&path)?;
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let task = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["task"].as_str().map(String::from));
    Ok(Some((name, task.unwrap_or_default())))
}

/// Runs one task a client sent; the reply carries the turn's verdict, report rows and answer, or why there is none.
async fn answer(agent: &mut Agent, task: &str) -> Value {
    match task.trim() {
        "/stop" => json!({"stopped": true, "status": "stopped server", "exit": 0}),
        "" => agent::failed(&Value::Null, &anyhow::anyhow!("the task is empty")),
        task if crate::COMMANDS.contains(&task.split_whitespace().next().unwrap_or_default()) => {
            agent::failed(&Value::Null, &anyhow::anyhow!("{task} is a REPL command; low --send takes tasks and /stop"))
        }
        task => match crate::unknown(task) {
            Some(why) => agent::failed(&Value::Null, &anyhow::anyhow!(why)),
            None => match agent.run(task).await {
                Ok(_) => agent.last().clone(),
                Err(e) => agent::failed(agent.last(), &e),
            },
        },
    }
}

/// Whether a low process holds the session's lock, as a server does while it runs.
pub fn locked(session: &Path) -> bool {
    File::options().append(true).open(session).is_ok_and(|f| f.try_lock().is_err())
}

/// The newest session served for this repository, or the one named; stale inboxes of servers that died are removed on
/// the way.
fn served(home: &Path, root: &Path, id: Option<&str>) -> Option<PathBuf> {
    let mut inboxes: Vec<PathBuf> = fs::read_dir(home.join(".low/sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "inbox"))
        .filter(|p| id.is_none_or(|id| p.file_stem().is_some_and(|s| s == id)))
        .collect();
    inboxes.sort();
    inboxes.into_iter().rev().find_map(|inbox| {
        let session = inbox.with_extension("jsonl");
        // Only the header's first line: the server owns the file.
        let header = File::open(&session)
            .ok()
            .and_then(|f| std::io::BufReader::new(f).lines().next()?.ok())
            .and_then(|line| serde_json::from_str::<Value>(&line).ok());
        if header.is_none_or(|h| h["cwd"] != json!(root)) {
            return None;
        }
        if !locked(&session) {
            let _ = fs::remove_dir_all(&inbox);
            return None;
        }
        Some(session)
    })
}

/// Gives the task to the newest session served for this repository (or the one named) and prints its result as a
/// one-shot run would, or as JSON; returns the result.
pub async fn send(home: &Path, root: &Path, id: Option<&str>, task: &str, json: bool) -> Result<Value> {
    let Some(session) = served(home, root, id) else {
        match id {
            Some(id) => {
                bail!("session {id} is not served in {}: start it with low --session {id} --serve", root.display())
            }
            None => bail!("no low --serve is running in {}: start one there first", root.display()),
        }
    };
    let dir = session.with_extension("inbox");
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let name = format!("{stamp:024}-{}", std::process::id());
    let temp = dir.join(format!("{name}.part"));
    fs::write(&temp, json!({"task": task}).to_string())
        .and_then(|()| fs::rename(&temp, dir.join(format!("{name}.task"))))?;
    let done = dir.join(format!("{name}.result"));
    let reply: Value = loop {
        if let Ok(text) = fs::read_to_string(&done) {
            let _ = fs::remove_file(&done);
            break serde_json::from_str(&text).context("the server wrote a result low cannot read")?;
        }
        if !locked(&session) {
            bail!("the low --serve for this repository stopped before it answered");
        }
        tokio::time::sleep(POLL).await;
    };
    if json {
        println!("{reply}");
        return Ok(reply);
    }
    if let Some(error) = reply["error"].as_str() {
        eprintln!("error: {error}");
        return Ok(reply);
    }
    if reply["stopped"] == true {
        eprintln!("[the server stopped]");
        return Ok(reply);
    }
    if let Some(answer) = reply["answer"].as_str().filter(|a| !a.trim().is_empty()) {
        let mut markdown = crate::render::Markdown::default();
        markdown.push(answer);
        markdown.finish();
    }
    let rows: Vec<(String, String)> = serde_json::from_value(reply["rows"].clone()).unwrap_or_default();
    let rows: Vec<(&str, String)> = rows.iter().map(|(label, value)| (label.as_str(), value.clone())).collect();
    crate::ui::report(&rows, reply["verdict"].as_str().unwrap_or("?"), reply["good"] == true);
    Ok(reply)
}
