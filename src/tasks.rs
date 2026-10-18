//! `low --tasks FILE` works through a markdown checklist in one session. A task is a line `- [ ] text` at the start of a
//! line; low marks it `[x]` when done, `[!]` when not verified and `[?]` when the model asked a question, and puts its
//! result under it as `  - low: …`. An `  - answer: …` line under a `[?]` task is sent as the next turn, then marked
//! `answered:`. The file is read again before each write, so edits made while low runs stay. Its first line names the
//! session, so the next run continues it. Text above the first task goes to the model with the first task of a run, and a
//! task that a turn limit cut off while it was still changing files gets up to two more turns.
use crate::agent::{self, Agent};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;

const SESSION: &str = "<!-- low session: ";
/// How many changed files a task's result line names.
const SHOWN: usize = 8;
/// How many more turns a task stopped at a limit gets, as long as each one changes files.
const AGAIN: usize = 2;

struct Task {
    /// The task's line in the file.
    line: usize,
    mark: char,
    text: String,
    /// Unused answers under it.
    answers: Vec<String>,
}

fn task(line: &str) -> Option<(char, &str)> {
    let rest = line.strip_prefix("- [")?;
    let mark = rest.chars().next().filter(|c| " xX!?".contains(*c))?;
    let text = rest[mark.len_utf8()..].strip_prefix("] ")?.trim();
    (!text.is_empty()).then_some((mark, text))
}

fn tasks(text: &str) -> Vec<Task> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = vec![];
    for (i, line) in lines.iter().enumerate() {
        let Some((mark, text)) = task(line) else { continue };
        let notes = lines[i + 1..].iter().take_while(|l| l.starts_with(char::is_whitespace));
        let answers = notes.filter_map(|l| l.trim_start().strip_prefix("- answer:")).map(|a| a.trim().to_string());
        out.push(Task { line: i, mark, text: text.into(), answers: answers.collect() });
    }
    out
}

/// The session the file names, if it is still there.
pub fn session(home: &Path, file: &Path) -> Option<String> {
    let first = std::fs::read_to_string(file).ok()?.lines().next()?.to_string();
    let id = first.strip_prefix(SESSION)?.strip_suffix(" -->")?.trim().to_string();
    home.join(format!(".low/sessions/{id}.jsonl")).exists().then_some(id)
}

/// Rewrites the task with this text: its mark, its `low:` line, and its answers marked used.
fn update(file: &Path, text: &str, mark: char, result: &str, id: &str) -> Result<()> {
    let old = std::fs::read_to_string(file)?;
    let mut lines: Vec<String> = old.lines().map(String::from).collect();
    if lines.first().is_some_and(|l| l.starts_with(SESSION)) {
        lines.remove(0);
    }
    lines.insert(0, format!("{SESSION}{id} -->"));
    if let Some(at) = lines.iter().position(|l| task(l).is_some_and(|(_, t)| t == text)) {
        lines[at] = format!("- [{mark}] {text}");
        let mut end = at + 1;
        while end < lines.len() && lines[end].starts_with(char::is_whitespace) {
            if lines[end].trim_start().starts_with("- low:") {
                lines.remove(end);
                continue;
            }
            if let Some(answer) = lines[end].trim_start().strip_prefix("- answer:") {
                lines[end] = format!("  - answered:{answer}");
            }
            end += 1;
        }
        lines.insert(at + 1, format!("  - low: {result}"));
    }
    let temp = file.with_extension("low-part");
    std::fs::write(&temp, lines.join("\n") + "\n")?;
    std::fs::rename(&temp, file).with_context(|| format!("could not update {}", file.display()))
}

/// One line for a task's result.
fn summary(result: &Value) -> String {
    if let Some(error) = result["error"].as_str() {
        return format!("error: {}", error.lines().next().unwrap_or_default());
    }
    if let Some(question) = result["question"].as_str() {
        return format!("question: {}", question.replace('\n', " "));
    }
    let secs = result["ms"].as_u64().unwrap_or(0) / 1000;
    let mut line = format!(
        "{} · {} requests · {}:{:02}",
        result["verdict"].as_str().unwrap_or("?"),
        result["requests"],
        secs / 60,
        secs % 60
    );
    let changes: Vec<&str> = result["changes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !changes.is_empty() {
        line += &format!(" · {}", changes[..changes.len().min(SHOWN)].join(", "));
    }
    if changes.len() > SHOWN {
        line += &format!(" and {} more", changes.len() - SHOWN);
    }
    line
}

/// The checklist's text before its first task, without low's session line: what every task in it is about.
fn preamble(text: &str) -> String {
    let lines = text.lines().filter(|l| !l.starts_with(SESSION));
    lines.take_while(|l| task(l).is_none()).collect::<Vec<_>>().join("\n").trim().to_string()
}

/// Runs the open tasks in order and stops at the first that is not done; the result lists each task's.
pub async fn run(agent: &mut Agent, file: &Path) -> Result<Value> {
    let id = agent.path().file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    // low rewrites the checklist after each task; the next turn need not hear about it.
    agent.quiet(file);
    let mut done = vec![];
    // The text above the tasks goes to the model once per run, with the first task.
    let read =
        |file: &Path| std::fs::read_to_string(file).with_context(|| format!("could not read {}", file.display()));
    let mut context = Some(preamble(&read(file)?)).filter(|p| !p.is_empty());
    let (mut last, mut again) = (String::new(), 0);
    loop {
        let text = read(file)?;
        let Some(next) = tasks(&text).into_iter().find(|t| !matches!(t.mark, 'x' | 'X')) else { break };
        if next.text != last {
            (last, again) = (next.text.clone(), 0);
        }
        let prompt = match (next.mark, next.answers.is_empty()) {
            ('?', true) => {
                let why = format!("task on line {} waits for an answer: add `  - answer: …` under it", next.line + 1);
                done.push(json!({"task": next.text, "status": "question", "exit": 4, "waiting": why}));
                eprintln!("{why}");
                break;
            }
            ('?', false) => next.answers.join("\n"),
            _ => match context.take() {
                Some(context) => format!("{}\n\nWritten above this task and the ones after it:\n{context}", next.text),
                None => next.text.clone(),
            },
        };
        let result = match agent.run(&prompt).await {
            Ok(_) => agent.last().clone(),
            Err(e) => agent::failed(agent.last(), &e),
        };
        let mark = match result["status"].as_str() {
            Some("done") => 'x',
            Some("question") => '?',
            _ => '!',
        };
        update(file, &next.text, mark, &summary(&result), &id)?;
        // A task cut off by a turn limit that was still changing files continues in a fresh turn of the same session.
        let progressed = result["changes"].as_array().is_some_and(|c| !c.is_empty());
        let limited = result["status"] == "stopped" && result["limit"].is_string();
        let more = mark != 'x' && limited && progressed && again < AGAIN;
        let stop = mark != 'x' && !more;
        again += usize::from(more);
        done.push(json!({"task": next.text, "result": result}));
        if stop {
            break;
        }
    }
    let exit = done.last().map_or(0, |t| t["result"]["exit"].as_i64().or(t["exit"].as_i64()).unwrap_or(0));
    let left = tasks(&std::fs::read_to_string(file)?).iter().filter(|t| !matches!(t.mark, 'x' | 'X')).count();
    eprintln!("[tasks: {} run, {left} left in {}]", done.len(), file.display());
    Ok(json!({"session": agent.path(), "id": id, "tasks": done, "left": left, "exit": exit}))
}
