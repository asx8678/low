//! `low --serve` keeps one session open between tasks and takes each next task from `low --send`. A client leaves its
//! task as a file in an inbox folder next to the session log and waits for the result file (a Unix socket there could
//! pass the 104-byte path limit). The server holds the session's lock while it runs, which is how a client tells a
//! live server from one that died. While it waits, the server keeps the provider's prompt cache warm (Agent::warm).
//! The server keeps `status.json` in the inbox current, so a program driving it can watch a task without asking: what
//! it is doing while it runs (ui::status), and between tasks how the last one ended.
use crate::agent::{self, Agent};
use crate::ui;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, DirBuilder, File};
use std::io::BufRead;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the server looks for a task, and a client for its result.
const POLL: Duration = Duration::from_millis(250);
/// How often the status file is written again when nothing in it changed, so its `at` shows the server is alive.
const BEAT: Duration = Duration::from_secs(1);
/// How long after the last task the server keeps the prompt cache warm: past it, a warm request is spent on a session
/// that may never get another task.
const WARM_FOR: Duration = Duration::from_secs(3600);

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
    let mut beat = Beat::default();
    beat.write(&dir, json!({"state": "idle", "tasks": 0}));
    eprintln!("[serving: send tasks with low --send \"<task>\" in this repository; low --send /stop ends it]");
    if let Some(task) = first
        && let Err(e) = agent.run(&task).await
    {
        eprintln!("error: {}", agent::error_text(&e));
    }
    let (mut idle, mut last, mut warming) = (tokio::time::Instant::now(), tokio::time::Instant::now(), true);
    let (mut tasks, mut ended) = (0u64, json!({"state": "idle", "tasks": 0}));
    let mut poll = tokio::time::interval(POLL);
    loop {
        tokio::select! {
            _ = poll.tick() => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        if let Some((name, task)) = next(&dir)? {
            ui::begin(&task);
            let mut reply = tokio::select! {
                reply = answer(agent, &task) => reply,
                () = controls(&dir, &mut beat) => unreachable!("controls never returns"),
            };
            if reply["stopped"] != true {
                reply["steers"] = ui::end();
            }
            // A steer or an abort the task did not take is for that task alone.
            ui::abort(false);
            ui::unsteer();
            clear_controls(&dir);
            tasks += 1;
            let outcome = json!({"status": reply["status"], "exit": reply["exit"], "verdict": reply["verdict"]});
            ended = json!({"state": "idle", "tasks": tasks, "since": ui::now_ms(), "last": outcome});
            beat.write(&dir, ended.clone());
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
            (idle, last, warming) = (tokio::time::Instant::now(), tokio::time::Instant::now(), true);
            continue;
        }
        // A steer or an abort that comes while no task runs, as one sent while the last task ended does, is for no
        // task: the next one must not take it. A task left with it is picked up before this poll clears anything.
        clear_controls(&dir);
        beat.write(&dir, ended.clone());
        if warming && idle.elapsed() >= agent::WARM_EVERY {
            if last.elapsed() >= WARM_FOR {
                crate::ui::note("no task for an hour: the prompt cache is no longer kept warm until the next one");
                warming = false;
                continue;
            }
            let mut warm = ended.clone();
            warm["state"] = "warming".into();
            beat.write(&dir, warm);
            if let Err(e) = agent.warm().await {
                crate::ui::note(&format!("could not keep the prompt cache warm: {}", agent::error_text(&e)));
            }
            idle = tokio::time::Instant::now();
        }
    }
}

/// While a task runs, takes the steers and aborts that `low --send "/steer <text>"` and `low --send /abort` leave in the
/// inbox: a steer goes with the turn's next request, an abort stops the turn as Ctrl-C does. One that comes between
/// tasks is dropped. It keeps the status file current meanwhile.
async fn controls(dir: &Path, beat: &mut Beat) {
    loop {
        beat.write(dir, ui::status());
        let mut found: Vec<PathBuf> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "steer" || e == "abort"))
            .collect();
        found.sort();
        for path in found {
            let text = fs::read_to_string(&path).unwrap_or_default();
            let _ = fs::remove_file(&path);
            if path.extension().is_some_and(|e| e == "abort") {
                ui::abort(true);
            } else if let Some(steer) =
                serde_json::from_str::<Value>(&text).ok().and_then(|v| v["text"].as_str().map(String::from))
            {
                ui::steer(&steer);
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The status file's last content, written again when it changes or a BEAT has passed.
#[derive(Default)]
struct Beat(Option<(Value, tokio::time::Instant)>);

impl Beat {
    fn write(&mut self, dir: &Path, status: Value) {
        if self.0.as_ref().is_some_and(|(last, at)| *last == status && at.elapsed() < BEAT) {
            return;
        }
        let mut file = status.clone();
        file["at"] = ui::now_ms().into();
        file["pid"] = std::process::id().into();
        let temp = dir.join("status.part");
        // Written whole, then renamed, as a result is. A write that fails is tried again on the next poll.
        if fs::write(&temp, format!("{file}\n")).and_then(|()| fs::rename(&temp, dir.join("status.json"))).is_ok() {
            self.0 = Some((status, tokio::time::Instant::now()));
        }
    }
}

fn clear_controls(dir: &Path) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.path().extension().is_some_and(|e| e == "steer" || e == "abort") {
            let _ = fs::remove_file(entry.path());
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
    let refused = |why: String| json!(agent::failed(None, &anyhow::anyhow!(why)));
    match task.trim() {
        "/stop" => json!({"stopped": true, "status": "stopped server", "exit": 0}),
        "" => refused("the task is empty".into()),
        task if crate::COMMANDS.contains(&task.split_whitespace().next().unwrap_or_default()) => {
            refused(format!("{task} is a REPL command; low --send takes tasks and /stop"))
        }
        task => match crate::unknown(task) {
            Some(why) => refused(why),
            None => {
                crate::ui::task(task);
                json!(agent.task(task).await)
            }
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
    if task.trim() == "/status" {
        let text = fs::read_to_string(dir.join("status.json")).context("the server has written no status yet")?;
        let status: Value = serde_json::from_str(&text).context("the server wrote a status low cannot read")?;
        println!("{}", if json { status.to_string() } else { serde_json::to_string_pretty(&status)? });
        return Ok(json!({"exit": 0}));
    }
    // A steer or an abort is left for the running task and not waited on.
    let control = match task.trim() {
        "/abort" => Some(("abort", String::new())),
        "/steer" => bail!("/steer needs a message: low --send \"/steer <text>\""),
        task => task
            .strip_prefix("/steer")
            .filter(|t| t.starts_with(char::is_whitespace))
            .map(|t| ("steer", t.trim().to_string())),
    };
    if let Some((kind, text)) = control {
        let temp = dir.join(format!("{name}.part"));
        fs::write(&temp, json!({"text": text}).to_string())
            .and_then(|()| fs::rename(&temp, dir.join(format!("{name}.{kind}"))))?;
        let reply = json!({"status": format!("{kind} sent"), "exit": 0});
        match json {
            true => println!("{reply}"),
            false => eprintln!("[{kind} sent]"),
        }
        return Ok(reply);
    }
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
    if reply["stopped"] == true {
        eprintln!("[the server stopped]");
        return Ok(reply);
    }
    let outcome = agent::Outcome::deserialize(&reply).context("the server wrote a result low cannot read")?;
    if let Some(error) = &outcome.error {
        eprintln!("error: {error}");
        return Ok(reply);
    }
    let Some(turn) = &outcome.turn else { return Ok(reply) };
    if let Some(answer) = turn.answer.as_deref().filter(|a| !a.trim().is_empty()) {
        let mut markdown = crate::render::Markdown::default();
        markdown.push(answer);
        markdown.finish();
    }
    crate::ui::report(&turn.rows, &turn.verdict, turn.good);
    Ok(reply)
}
