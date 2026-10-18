//! `low --tasks FILE` works through a markdown checklist in one session. A task is a line `- [ ] text` at the start of a
//! line; low marks it `[x]` when done, `[!]` when not verified and `[?]` when the model asked a question, and puts its
//! result under it as `  - low: …`. An `  - answer: …` line under a `[?]` task is sent as the next turn, then marked
//! `answered:`. The file is read again before each write, so edits made while low runs stay; a task is found again by
//! its line, or by its text if those edits moved it, so identical tasks each get their own mark. Its first line names
//! the session, so the next run continues it. Text above the first task goes to the model with the first task of a run,
//! and a task that a turn limit cut off while it was still changing files gets up to two more turns.
use crate::agent::{Agent, Outcome};
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

/// Rewrites the task that was on `line` with this text: its mark, its `low:` line, and its answers marked used. Edits
/// made meanwhile can move it, so it is the line with this text nearest to `line`, an open one first: found by its text
/// alone, the first of two identical tasks was marked again and again while the second ran forever.
fn update(file: &Path, line: usize, text: &str, mark: char, result: &str, id: &str) -> Result<()> {
    let old = std::fs::read_to_string(file)?;
    let mut lines: Vec<String> = old.lines().map(String::from).collect();
    // `line` counts the session line, which goes on top here if it isn't there yet.
    let line = if lines.first().is_some_and(|l| l.starts_with(SESSION)) {
        lines.remove(0);
        line
    } else {
        line + 1
    };
    lines.insert(0, format!("{SESSION}{id} -->"));
    let nearest = |open: bool| {
        let this = |l: &&String| task(l).is_some_and(|(m, t)| t == text && !(open && matches!(m, 'x' | 'X')));
        lines.iter().enumerate().filter(|(_, l)| this(l)).min_by_key(|(i, _)| i.abs_diff(line)).map(|(i, _)| i)
    };
    if let Some(at) = nearest(true).or_else(|| nearest(false)) {
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
    // The rewrite keeps the file's line ends and permissions.
    let eol = if old.contains("\r\n") { "\r\n" } else { "\n" };
    let temp = file.with_extension("low-part");
    std::fs::write(&temp, lines.join(eol) + eol)?;
    std::fs::set_permissions(&temp, std::fs::metadata(file)?.permissions())?;
    std::fs::rename(&temp, file).with_context(|| format!("could not update {}", file.display()))
}

/// One line for a task's result.
fn summary(result: &Outcome) -> String {
    if let Some(error) = &result.error {
        return format!("error: {}", error.lines().next().unwrap_or_default());
    }
    let Some(turn) = &result.turn else { return "?".into() };
    if let Some(question) = &turn.question {
        return format!("question: {}", question.replace('\n', " "));
    }
    let secs = turn.ms / 1000;
    let mut line = format!("{} · {} requests · {}:{:02}", turn.verdict, turn.requests, secs / 60, secs % 60);
    let changes = &turn.changes;
    if !changes.is_empty() {
        line += &format!(" · {}", changes[..changes.len().min(SHOWN)].join(", "));
    }
    if changes.len() > SHOWN {
        line += &format!(" and {} more", changes.len() - SHOWN);
    }
    if let Some(limit) = &turn.limit {
        line += &format!(" · {limit}");
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
    let (mut last, mut again, mut exit) = (String::new(), 0, 0);
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
                exit = 4;
                break;
            }
            ('?', false) => next.answers.join("\n"),
            _ => match context.take() {
                Some(context) => format!("{}\n\nWritten above this task and the ones after it:\n{context}", next.text),
                None => next.text.clone(),
            },
        };
        let result = agent.task(&prompt).await;
        let mark = match result.status.as_str() {
            "done" => 'x',
            "question" => '?',
            _ => '!',
        };
        update(file, next.line, &next.text, mark, &summary(&result), &id)?;
        // A task cut off by a turn limit that was still changing files continues in a fresh turn of the same session,
        // unless the session has spent its budget.
        let turn = result.turn.as_ref();
        let progressed = turn.is_some_and(|t| !t.changes.is_empty());
        let limited = result.status == "stopped" && turn.is_some_and(|t| t.limit.is_some());
        let more = mark != 'x' && limited && progressed && again < AGAIN && agent.over_budget().is_none();
        let stop = mark != 'x' && !more;
        again += usize::from(more);
        exit = result.exit;
        done.push(json!({"task": next.text, "result": result}));
        if stop {
            break;
        }
    }
    let left = tasks(&std::fs::read_to_string(file)?).iter().filter(|t| !matches!(t.mark, 'x' | 'X')).count();
    eprintln!("[tasks: {} run, {left} left in {}]", done.len(), file.display());
    Ok(json!({"session": agent.path(), "id": id, "tasks": done, "left": left, "exit": exit}))
}
