use rustix::process::{Pid, Signal, kill_process_group};
use rustix::stdio::{stdin, stdout};
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

struct Bar {
    rows: u16,
    cols: u16,
    started: Instant,
    activity: String,
    tokens: (u64, u64),
}

static BAR: Mutex<Option<Bar>> = Mutex::new(None);
static STEP: AtomicUsize = AtomicUsize::new(0);
static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static TERMIOS: OnceLock<Termios> = OnceLock::new();
static BARRED: AtomicBool = AtomicBool::new(false);
pub static CHECK: AtomicI32 = AtomicI32::new(0);

static STYLED: LazyLock<bool> =
    LazyLock::new(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());

pub fn styled() -> bool {
    *STYLED
}

pub fn paint(code: &str, text: &str) -> String {
    if styled() { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
}

/// The longest start of `text` that fits in `cols` terminal columns: wide characters (CJK, most emoji) take two.
pub fn fit(text: &str, cols: usize) -> &str {
    let mut used = 0;
    for (i, c) in text.char_indices() {
        used += c.width().unwrap_or(0);
        if used > cols {
            return &text[..i];
        }
    }
    text
}

pub fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .filter(|c| !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}' | '\u{feff}'))
        .collect()
}

/// The terminal's rows and columns, asked of the tty itself: the ticker calls this every second.
fn size() -> Option<(u16, u16)> {
    let ws = tcgetwinsize(std::fs::File::open("/dev/tty").ok()?).ok()?;
    Some((ws.ws_row, ws.ws_col)).filter(|&(rows, cols)| rows >= 5 && cols >= 20)
}

pub struct Turn(Option<tokio::task::JoinHandle<()>>);

impl Turn {
    pub fn start() -> Self {
        STEP.store(0, Ordering::Relaxed);
        *PLAN.lock().unwrap() = Default::default();
        LIVE.lock().unwrap().intent = None;
        let Some((rows, cols)) = size().filter(|_| styled()) else { return Turn(None) };
        reserve(rows);
        let started = Instant::now();
        *BAR.lock().unwrap() =
            Some(Bar { rows, cols, started, activity: "waiting for the model".into(), tokens: (0, 0) });
        draw();
        BARRED.store(true, Ordering::Relaxed);
        Turn(Some(tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                resize();
                draw();
            }
        })))
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if let Some(ticker) = self.0.take() {
            ticker.abort();
            BARRED.store(false, Ordering::Relaxed);
            if let Some(bar) = BAR.lock().unwrap().take() {
                print!("\x1b7\x1b[{};1H\x1b[2K\x1b[r\x1b8", bar.rows);
                let _ = std::io::stdout().flush();
            }
        }
    }
}

pub fn guard() {
    if let Ok(saved) = tcgetattr(stdin()) {
        let _ = TERMIOS.set(saved);
    }
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        if unsafe { libc::signal(signal, quit as *const () as libc::sighandler_t) } == libc::SIG_IGN {
            unsafe { libc::signal(signal, libc::SIG_IGN) };
        }
    }
}

extern "C" fn quit(signal: libc::c_int) {
    let group = CHECK.load(Ordering::Relaxed);
    let reset: &[u8] = if BARRED.load(Ordering::Relaxed) { b"\x1b7\x1b[999;1H\x1b[2K\x1b[r\x1b8\n" } else { b"" };
    // Only async-signal-safe calls from here: each is one system call.
    if group > 1
        && let Some(group) = Pid::from_raw(group)
    {
        let _ = kill_process_group(group, Signal::KILL);
    }
    if let Some(saved) = TERMIOS.get() {
        let _ = tcsetattr(stdin(), OptionalActions::Now, saved);
    }
    let _ = rustix::io::write(stdout(), reset);
    unsafe { libc::_exit(128 + signal) };
}

fn reserve(rows: u16) {
    print!("\n\x1b[1A\x1b7\x1b[1;{}r\x1b8", rows.saturating_sub(1));
}

fn resize() {
    let Some((rows, cols)) = size() else { return };
    let mut bar = BAR.lock().unwrap();
    if let Some(bar) = bar.as_mut().filter(|b| (b.rows, b.cols) != (rows, cols)) {
        (bar.rows, bar.cols) = (rows, cols);
        reserve(rows);
    }
}

fn draw() {
    let bar = BAR.lock().unwrap();
    let Some(bar) = bar.as_ref() else { return };
    let secs = bar.started.elapsed().as_secs();
    let (prompt, cached) = bar.tokens;
    let cache = (cached * 100).checked_div(prompt).map(|p| format!(" · {p}% cached")).unwrap_or_default();
    let line = format!(" ▸ {} │ {}{cache} │ {} ", bar.activity, tokens(prompt), minutes(secs));
    let line = fit(&line, bar.cols.saturating_sub(1) as usize);
    print!("\x1b7\x1b[{};1H\x1b[2K\x1b[7m{line}\x1b[0m\x1b8", bar.rows);
    let _ = std::io::stdout().flush();
}

fn update(change: impl FnOnce(&mut Bar)) {
    if let Some(bar) = BAR.lock().unwrap().as_mut() {
        change(bar);
    }
    draw();
}

/// Seconds as m:ss.
pub fn minutes(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// A token count as 950 tok, 99.7k tok, 1.2M tok or 2.7B tok: the largest unit that still shows under 1000.
pub fn tokens(n: u64) -> String {
    if n < 1000 {
        return format!("{n} tok");
    }
    let mut value = n as f64 / 1000.0;
    for unit in ["k", "M"] {
        // Rounded to one decimal, 999.96 would read 1000.0.
        if (value * 10.0).round() < 10_000.0 {
            return format!("{value:.1}{unit} tok");
        }
        value /= 1000.0;
    }
    format!("{value:.1}B tok")
}

pub fn activity(text: &str) {
    let mut bar = BAR.lock().unwrap();
    match bar.as_mut() {
        Some(bar) if bar.activity != text => bar.activity = text.to_string(),
        _ => return,
    }
    drop(bar);
    draw();
}

/// Shows text as the bar's activity until dropped, then puts back what the bar showed before.
pub struct Busy(Option<String>);

pub fn busy(text: &str) -> Busy {
    let before = BAR.lock().unwrap().as_mut().map(|bar| std::mem::replace(&mut bar.activity, text.into()));
    draw();
    Busy(before)
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(before) = self.0.take() {
            activity(&before);
        }
    }
}

pub fn cached(usage: &Value) -> Option<u64> {
    let cached = usage["prompt_tokens_details"]["cached_tokens"].as_u64();
    cached.or(usage["cached_tokens"].as_u64()).or(usage["prompt_cache_hit_tokens"].as_u64())
}

pub fn usage(usage: &Value) {
    let cached = cached(usage);
    let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
    update(|bar| bar.tokens = (bar.tokens.0 + prompt, bar.tokens.1 + cached.unwrap_or(0)));
}

pub fn width() -> usize {
    BAR.lock().unwrap().as_ref().map_or(120, |b| b.cols as usize)
}

fn flat(text: &str, max: usize) -> String {
    let flat = printable(text).split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.width() <= max { flat } else { format!("{}…", fit(&flat, max)) }
}

/// A note cut to one line of the terminal.
pub fn line(text: &str) -> String {
    flat(text, width().saturating_sub(3).max(20))
}

/// Shows a task that wasn't typed here (a --tasks checklist line, a task from low --send) as the REPL's prompt shows a
/// typed one, on one line.
pub fn task(text: &str) {
    let cols = size().map_or(120, |(_, cols)| cols as usize);
    println!("› {}", flat(text, cols.saturating_sub(3).max(20)));
}

/// The first line of code that says what it does: not blank, a comment or an import.
pub fn headline(code: &str) -> Option<&str> {
    let skip = |l: &&str| l.is_empty() || l.starts_with('#') || l.starts_with("import ") || l.starts_with("from ");
    code.lines().map(str::trim).find(|l| !skip(l))
}

struct Live {
    drawn: usize,
    at: Option<Instant>,
    rows: Vec<(&'static str, String)>,
    group: Option<(usize, Vec<String>)>,
    intent: Option<String>,
    running: Option<(Instant, String)>,
}

static LIVE: Mutex<Live> =
    Mutex::new(Live { drawn: 0, at: None, rows: Vec::new(), group: None, intent: None, running: None });
static PLAN: Mutex<(Vec<String>, Vec<u64>)> = Mutex::new((Vec::new(), Vec::new()));
const LIVE_LINES: usize = 3;
const INSPECT: &str = "cat|head|tail|sed|nl|wc|ls|tree|find|grep|rg|file|stat|cd|pwd|echo|diff|sort|uniq|cut|awk|jq|\
                       which|du|git log|git show|git diff|git status|git grep|git blame|git ls-files|git ls-tree|\
                       git rev-parse";

fn inspects(cmd: &str) -> bool {
    let read = |part: &str| {
        INSPECT.split('|').any(|i| part.strip_prefix(i).is_some_and(|rest| rest.is_empty() || rest.starts_with(' ')))
    };
    cmd.replace("&&", ";").split([';', '|', '\n']).map(str::trim).filter(|p| !p.is_empty()).all(read)
}

fn redraw(live: &mut Live) {
    let Some(cols) = BAR.lock().unwrap().as_ref().map(|b| b.cols as usize) else { return };
    let group = live.group.as_ref().map(|(runs, files)| ("2", grouped(*runs, files)));
    let mut out = if live.drawn > 0 { format!("\x1b[{}A", live.drawn) } else { String::new() };
    let mut drawn = 0;
    for (style, text) in group.iter().chain(&live.rows) {
        let text = fit(text, cols.saturating_sub(3));
        out += &format!("\r\x1b[2K  {}\n", paint(style, text));
        drawn += 1;
    }
    let stale = live.drawn.saturating_sub(drawn);
    if stale > 0 {
        out += &format!("{}\x1b[{stale}A", "\r\x1b[2K\n".repeat(stale));
    }
    print!("{out}");
    let _ = std::io::stdout().flush();
    live.drawn = drawn;
}

fn erase(live: &mut Live) {
    let n = std::mem::take(&mut live.drawn);
    if n > 0 {
        print!("\x1b[{n}A{}\x1b[{n}A", "\r\x1b[2K\n".repeat(n));
        let _ = std::io::stdout().flush();
    }
}

pub fn settle() {
    let mut live = LIVE.lock().unwrap();
    erase(&mut live);
    live.rows.clear();
    if let Some((runs, files)) = live.group.take() {
        println!("  {}", paint("2", &grouped(runs, &files)));
    }
}

fn grouped(runs: usize, files: &[String]) -> String {
    if files.is_empty() {
        return format!("◦ {runs} run{}", if runs == 1 { "" } else { "s" });
    }
    let count = if runs > 1 { format!(" · {runs} runs") } else { String::new() };
    let room = width().saturating_sub(16 + count.len()).max(20);
    let (mut list, mut shown) = (String::new(), 0);
    for file in files {
        if shown > 0 && list.width() + 2 + file.width() > room {
            break;
        }
        list += &format!("{}{file}", if shown > 0 { ", " } else { "" });
        shown += 1;
    }
    let rest = if shown < files.len() { format!(" +{}", files.len() - shown) } else { String::new() };
    format!("◦ read {list}{rest}{count}")
}

pub fn thinking(text: &str, secs: u64) {
    activity(&format!("thinking {secs}s"));
    let Some(cols) = BAR.lock().unwrap().as_ref().map(|b| b.cols as usize) else { return };
    let mut live = LIVE.lock().unwrap();
    if live.at.is_some_and(|drawn| drawn.elapsed() < Duration::from_millis(80)) {
        return;
    }
    let text = printable(text);
    let width = cols.saturating_sub(6).max(20);
    let start = text.char_indices().rev().nth(width * (LIVE_LINES + 2)).map_or(0, |(i, _)| i);
    let lines = wrap(&text[start..].split_whitespace().collect::<Vec<_>>().join(" "), width);
    let tail = &lines[lines.len().saturating_sub(LIVE_LINES)..];
    let chars = text.chars().count();
    let size = if chars >= 1000 { format!("{:.1}k chars", chars as f64 / 1000.0) } else { format!("{chars} chars") };
    live.rows = vec![("35", format!("◇ thinking {secs}s · {size}"))];
    for i in 0..LIVE_LINES {
        live.rows.push(("2;3", format!("  {}", tail.get(i).map_or("", String::as_str))));
    }
    live.at = Some(Instant::now());
    redraw(&mut live);
}

pub fn writing(lines: usize) {
    activity("writing code");
    let mut live = LIVE.lock().unwrap();
    if BAR.lock().unwrap().is_none() || live.at.is_some_and(|drawn| drawn.elapsed() < Duration::from_millis(80)) {
        return;
    }
    live.rows = vec![("36", format!("✎ writing code · {lines} line{}", if lines == 1 { "" } else { "s" }))];
    live.at = Some(Instant::now());
    redraw(&mut live);
}

pub fn intent(said: &str, thinking: &str) {
    let text = if said.trim().is_empty() { thinking } else { said };
    let intent = match sentences(text).as_slice() {
        [] => None,
        [.., before, last] if last.chars().count() < 25 => Some(format!("{before} {last}")),
        [.., last] => Some(last.to_string()),
    };
    LIVE.lock().unwrap().intent = intent;
}

fn sentences(text: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next_is_space = chars.peek().is_none_or(|&(_, n)| n.is_whitespace());
        if c == '\n' || (matches!(c, '.' | '?' | '!') && next_is_space) {
            let end = i + c.len_utf8();
            out.push(text[start..end].trim());
            start = end;
        }
    }
    out.push(text[start..].trim());
    out.retain(|s| !s.is_empty());
    out
}

pub fn run(code: &str) {
    let step = STEP.fetch_add(1, Ordering::Relaxed) + 1;
    let code = printable(code);
    let first = headline(&code).unwrap_or("(no code)");
    activity(&format!("step {step} running"));
    let mut live = LIVE.lock().unwrap();
    let label = flat(&live.intent.take().unwrap_or_else(|| first.to_string()), 300);
    live.rows = vec![("2", format!("▸ {label}"))];
    live.running = Some((Instant::now(), label));
    redraw(&mut live);
}

pub fn ran(meta: &Value, out: &str) {
    let strings = |v: &Value| -> Vec<String> {
        v.as_array().into_iter().flatten().filter_map(Value::as_str).map(|s| flat(s, 120)).collect()
    };
    let mut reads = strings(&meta["reads"]);
    let mut cmds = vec![];
    for cmd in meta["cmds"].as_array().into_iter().flatten() {
        let text = cmd[0].as_str().unwrap_or_default().trim();
        if inspects(text) {
            reads.extend(strings(&cmd[2]));
        } else {
            cmds.push((text, &cmd[1]));
        }
    }
    let actions = meta["actions"].as_array().map_or(&[][..], Vec::as_slice);
    let mut live = LIVE.lock().unwrap();
    live.rows.clear();
    let (started, label) = live.running.take().unzip();
    let took = started.map(|s| s.elapsed().as_secs_f64()).filter(|&s| s >= 1.0);
    let quiet =
        cmds.is_empty() && actions.is_empty() && ["exception", "accepted", "plan"].iter().all(|k| meta[k].is_null());
    let probe = quiet && reads.is_empty() && meta["cmds"].as_array().is_none_or(Vec::is_empty);
    if probe && let Some(label) = label {
        drop(live);
        settle();
        let took = took.map(|s| format!(" {s:.1}s")).unwrap_or_default();
        println!("  {}", paint("2", &format!("▸ {}{took}", flat(&label, width().saturating_sub(12).max(30)))));
        return;
    }
    if quiet {
        let (runs, files) = live.group.get_or_insert_default();
        *runs += 1;
        for read in reads {
            if !files.contains(&read) {
                files.push(read);
            }
        }
        redraw(&mut live);
        return;
    }
    drop(live);
    settle();
    let room = width().saturating_sub(8).max(30);
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let (mut rows, mut timed) = (vec![], None);
    if let Some(criteria) = meta["accepted"].as_str() {
        rows.push(format!("{} {}", paint("35", "◆ criteria"), flat(criteria, room.saturating_sub(11))));
    }
    // What failed in a run is dim: the model reads it and goes on, and what is left failing at the end shows in red in
    // the report.
    let mut failed = false;
    for (i, act) in actions.iter().enumerate() {
        let (tool, ok) = (flat(act["tool"].as_str().unwrap_or_default(), 20), act["ok"] == true);
        // One row per file: the run's later edits and writes of it add to its first row.
        let same = |other: &&Value| other["ok"] == true && other["path"] == act["path"];
        match act["path"].as_str().map(|p| flat(p, 120)) {
            Some(_) if ok && actions[..i].iter().any(|other| same(&other)) => {}
            Some(path) if ok => {
                let sum =
                    |key: &str| actions[i..].iter().filter(same).map(|a| a[key].as_u64().unwrap_or(0)).sum::<u64>();
                let (a, r) = (paint("32", &format!("+{}", sum("added"))), paint("31", &format!("−{}", sum("removed"))));
                rows.push(format!("{} {tool} {path}  {a} {r}", paint("32", "✓")));
            }
            Some(path) => {
                let error = flat(act["error"].as_str().unwrap_or_default(), room.saturating_sub(20));
                rows.push(paint("2", &format!("✗ {tool} {path}: {error}")));
            }
            None => {
                rows.push(if ok {
                    format!("{} check passed", paint("32", "✓"))
                } else {
                    paint("2", "✗ check FAILED")
                });
                (timed, failed) = (Some(rows.len() - 1), failed || !ok);
            }
        }
    }
    if failed {
        rows.extend(lines[lines.len().saturating_sub(3)..].iter().map(|l| paint("2", &format!("  {}", flat(l, room)))));
    }
    for (i, (cmd, exit)) in cmds.iter().enumerate() {
        if i == 3 {
            rows.push(paint("2", &format!("  +{} more commands", cmds.len() - 3)));
            break;
        }
        let cmd = flat(cmd, room.saturating_sub(14));
        rows.push(match exit.as_i64() {
            Some(0) => format!("{} {cmd}  {}", paint("36", "$"), paint("32", "✓")),
            Some(code) => paint("2", &format!("$ {cmd}  ✗ exit {code}")),
            None => paint("2", &format!("$ {cmd}  ✗ {}", flat(exit.as_str().unwrap_or("failed"), 40))),
        });
        timed = Some(rows.len() - 1);
    }
    if meta["plan"].is_object() {
        rows.extend(plan(&meta["plan"]));
    }
    if let Some(name) = meta["exception"].as_str().filter(|_| actions.iter().all(|a| a["ok"] == true)) {
        let message = lines.iter().rev().find(|l| l.split(':').next().is_some_and(|head| head.ends_with(name)));
        rows.push(paint("2", &format!("✗ {}", flat(message.unwrap_or(&name), room))));
    }
    if let (Some(i), Some(secs)) = (timed, took) {
        rows[i] += &paint("2", &format!(" {secs:.1}s"));
    }
    for row in rows {
        println!("  {row}");
    }
}

pub fn prose() {
    settle();
    LIVE.lock().unwrap().intent = None;
    if STEP.load(Ordering::Relaxed) > 0 {
        println!();
    }
}

fn plan(plan: &Value) -> Vec<String> {
    let steps: Vec<String> = plan["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|s| flat(s, width().saturating_sub(14).max(30)))
        .collect();
    let done: Vec<u64> = plan["done"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    let current = (1..=steps.len() as u64).find(|i| !done.contains(i));
    let mut shown = PLAN.lock().unwrap();
    let rows = if shown.0 == steps {
        let fresh = (1u64..).zip(&steps).filter(|(i, _)| done.contains(i) && !shown.1.contains(i));
        fresh.map(|(i, step)| format!("{} {step}", paint("32", &format!("✓ step {i}/{}", steps.len())))).collect()
    } else {
        (1u64..)
            .zip(&steps)
            .map(|(i, step)| match i {
                i if done.contains(&i) => paint("32", &format!("  ✓ {step}")),
                i if Some(i) == current => paint("1", &format!("  ▸ {step}")),
                _ => paint("2", &format!("  · {step}")),
            })
            .collect()
    };
    *shown = (steps, done);
    rows
}

pub fn note(text: &str) {
    event("note", text, Value::Null);
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The milliseconds since `start`, as the session log records durations.
pub fn ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

pub fn event(kind: &str, text: &str, extra: Value) {
    show(text);
    record(kind, text, extra);
}

/// Prints a line as an event shows, without logging it.
pub fn show(text: &str) {
    settle();
    println!("{}", paint("33", &format!("· {}", printable(text))));
}

/// Logs an event to the session without showing it.
pub fn record(kind: &str, text: &str, extra: Value) {
    let mut entry = serde_json::json!({"event": kind, "text": text});
    if let (Some(entry), Value::Object(extra)) = (entry.as_object_mut(), extra) {
        entry.extend(extra);
    }
    log(entry);
}

/// Queues an entry for the session's next write, stamped now.
pub fn log(mut entry: Value) {
    entry["at"] = now_ms().into();
    EVENTS.lock().unwrap().push(entry);
}

pub fn events() -> Vec<Value> {
    std::mem::take(&mut *EVENTS.lock().unwrap())
}

/// Sends what low shows to /dev/null until dropped, so that with --json stdout carries only the result.
pub struct Quiet(rustix::fd::OwnedFd);

impl Quiet {
    pub fn start() -> std::io::Result<Self> {
        std::io::stdout().flush()?;
        let saved = rustix::io::dup(stdout())?;
        rustix::stdio::dup2_stdout(std::fs::File::options().write(true).open("/dev/null")?)?;
        Ok(Self(saved))
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        let _ = std::io::stdout().flush();
        let _ = rustix::stdio::dup2_stdout(&self.0);
    }
}

/// Prints a turn's report, without the rows the turn already showed: the task, the criteria, the plan's progress, and
/// the check that VERIFIED stands for. A --json result keeps them all.
pub fn report(rows: &[(impl AsRef<str>, String)], verdict: &str, good: bool) {
    settle();
    let head = format!("── {verdict} ") + &"─".repeat(36);
    println!("\n{}", paint(if good { "1;32" } else { "1;31" }, &head));
    let room = width().saturating_sub(12).max(30);
    let shown =
        |label: &str| !matches!(label, "task" | "criteria" | "plan") && (label != "check" || verdict != "VERIFIED");
    let rows = rows.iter().filter(|(label, value)| shown(label.as_ref()) && !matches!(value.as_str(), "-" | "none"));
    for (label, value) in rows {
        println!("{} {}", paint("2", &format!("{:<9}", label.as_ref())), wrap(value, room).join("\n          "));
    }
}

pub fn wrap(text: &str, room: usize) -> Vec<String> {
    let mut out = vec![];
    for line in printable(text).lines() {
        let mut current = String::new();
        for word in line.split_whitespace() {
            if !current.is_empty() && current.width() + 1 + word.width() > room {
                out.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        out.push(current);
    }
    out
}
