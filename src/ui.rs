use crate::picker::{self, Key};
use rustix::process::{Pid, Signal, kill_process_group};
use rustix::stdio::{stdin, stdout};
use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

struct Bar {
    rows: u16,
    cols: u16,
    started: Instant,
    activity: String,
    tokens: (u64, u64),
    /// The row above the bar shows what the user types to steer the turn.
    steering: bool,
}

static BAR: Mutex<Option<Bar>> = Mutex::new(None);
static STEP: AtomicUsize = AtomicUsize::new(0);
static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static TERMIOS: OnceLock<Termios> = OnceLock::new();
static BARRED: AtomicBool = AtomicBool::new(false);
/// The steering row is up above the bar.
static STEERING: AtomicBool = AtomicBool::new(false);
/// /models' picker is up, with the cursor hidden.
static PICKING: AtomicBool = AtomicBool::new(false);
pub static CHECK: AtomicI32 = AtomicI32::new(0);
/// The REPL reads what is typed while a turn runs; a task from the command line, --tasks or --serve leaves the keys be.
static STEERABLE: AtomicBool = AtomicBool::new(false);
static STEER: Mutex<Steer> = Mutex::new(Steer { typing: String::new(), sent: Vec::new() });

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
pub fn size() -> Option<(u16, u16)> {
    let ws = tcgetwinsize(std::fs::File::open("/dev/tty").ok()?).ok()?;
    Some((ws.ws_row, ws.ws_col)).filter(|&(rows, cols)| rows >= 5 && cols >= 20)
}

pub struct Turn(Option<tokio::task::JoinHandle<()>>, Option<Keys>);

impl Turn {
    pub fn start() -> Self {
        STEP.store(0, Ordering::Relaxed);
        *PLAN.lock().unwrap() = Default::default();
        LIVE.lock().unwrap().intent = None;
        let Some((rows, cols)) = size().filter(|_| styled()) else { return Turn(None, None) };
        let keys = STEERABLE.load(Ordering::Relaxed).then(Keys::start).flatten();
        let steering = keys.is_some();
        reserve(rows, steering);
        let started = Instant::now();
        *BAR.lock().unwrap() =
            Some(Bar { rows, cols, started, activity: "waiting for the model".into(), tokens: (0, 0), steering });
        draw();
        BARRED.store(true, Ordering::Relaxed);
        STEERING.store(steering, Ordering::Relaxed);
        let ticker = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                resize();
                draw();
            }
        });
        Turn(Some(ticker), keys)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        // The keys stop first, so nothing draws the rows once they are cleared.
        drop(self.1.take());
        if let Some(ticker) = self.0.take() {
            ticker.abort();
            BARRED.store(false, Ordering::Relaxed);
            STEERING.store(false, Ordering::Relaxed);
            if let Some(bar) = BAR.lock().unwrap().take() {
                let above = if bar.steering { format!("\x1b[{};1H\x1b[2K", bar.rows - 1) } else { String::new() };
                print!("\x1b7{above}\x1b[{};1H\x1b[2K\x1b[r\x1b8", bar.rows);
                let _ = std::io::stdout().flush();
            }
        }
    }
}

/// What the user types in the REPL while a turn runs: the line being typed, and the lines sent with Enter that no
/// request has taken yet.
struct Steer {
    typing: String,
    sent: Vec<String>,
}

impl Steer {
    fn key(&mut self, key: Key) {
        match key {
            Key::Char(c) => self.typing.push(c),
            Key::Paste(text) => self.typing.push_str(&text),
            Key::Backspace => drop(self.typing.pop()),
            Key::Clear => self.typing.clear(),
            Key::Enter => {
                let text = std::mem::take(&mut self.typing);
                // Blank lines around it go, but a paste's first line keeps its indentation, as the lines after it do.
                let start = text.find(|c: char| !c.is_whitespace()).map(|i| text[..i].rfind('\n').map_or(0, |n| n + 1));
                if let Some(start) = start {
                    self.sent.push(text[start..].trim_end().to_string());
                }
            }
            // Takes back the last line sent, while no request has taken it.
            Key::Up if self.typing.is_empty() => self.typing = self.sent.pop().unwrap_or_default(),
            _ => {}
        }
    }

    /// The lines sent before the first command, as one message: a command (/models) waits for the turn to end.
    fn take(&mut self) -> Option<String> {
        let lines = self.sent.iter().position(|l| command(l)).unwrap_or(self.sent.len());
        (lines > 0).then(|| self.sent.drain(..lines).collect::<Vec<_>>().join("\n"))
    }

    /// The row above the bar: the end of what is being typed, or how to steer.
    fn row(&self, cols: usize) -> String {
        let room = cols.saturating_sub(1);
        if self.typing.is_empty() {
            let hint = match self.sent.last() {
                None => "type to steer the model · Enter sends it after the current step".to_string(),
                Some(line) if command(line) => format!("{} runs when the turn ends", flat(line, room / 2)),
                Some(line) => format!("queued: {} · sent after the current step", flat(line, room / 2)),
            };
            return paint("2", fit(&format!("› {hint}"), room));
        }
        let queued = if self.sent.is_empty() { String::new() } else { format!("  {} queued", self.sent.len()) };
        let typed = printable(&self.typing).replace('\n', "⏎").replace('\t', " ");
        let shown = ending(&typed, room.saturating_sub(3 + queued.width()));
        format!("{} {shown}▏{}", paint("36", "›"), paint("2", &queued))
    }
}

/// A line the REPL takes as a command, or as a typo of one: its first word is /name. A path such as /src/main.rs is not,
/// and nor is a pasted block of lines.
fn command(line: &str) -> bool {
    !line.contains('\n')
        && line
            .split_whitespace()
            .next()
            .and_then(|word| word.strip_prefix('/'))
            .is_some_and(|name| !name.contains('/'))
}

/// The longest end of `text` that fits in `cols` terminal columns.
fn ending(text: &str, cols: usize) -> &str {
    let mut used = 0;
    for (i, c) in text.char_indices().rev() {
        used += c.width().unwrap_or(0);
        if used > cols {
            return &text[i + c.len_utf8()..];
        }
    }
    text
}

/// Reads what is typed during a turn on a thread of its own, with the terminal unbuffered and unechoed but Ctrl-C still
/// the signal that interrupts the turn. Dropping it stops the thread and puts the terminal back.
struct Keys {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    saved: Termios,
}

impl Keys {
    fn start() -> Option<Self> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        let saved = tcgetattr(stdin()).ok()?;
        let mut quiet = saved.clone();
        quiet.local_modes.remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::IEXTEN);
        quiet.special_codes[SpecialCodeIndex::VMIN] = 1;
        quiet.special_codes[SpecialCodeIndex::VTIME] = 0;
        tcsetattr(stdin(), OptionalActions::Now, &quiet).ok()?;
        // The terminal marks what is pasted, so a block of lines arrives as one message instead of a line each.
        print!("\x1b[?2004h");
        let _ = std::io::stdout().flush();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let stop = stop.clone();
            move || listen(&stop)
        });
        Some(Self { stop, thread: Some(thread), saved })
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        print!("\x1b[?2004l");
        let _ = std::io::stdout().flush();
        let _ = tcsetattr(stdin(), OptionalActions::Now, &self.saved);
    }
}

fn listen(stop: &AtomicBool) {
    let mut buf = vec![];
    let mut more = [0; 4096];
    while !stop.load(Ordering::Relaxed) {
        // Waking now and then notices the turn ended.
        if !picker::ready(100) {
            continue;
        }
        match rustix::io::read(stdin(), &mut more[..]) {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&more[..n]),
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return,
        }
        while picker::incomplete(&buf) && picker::ready(50) {
            match rustix::io::read(stdin(), &mut more[..]) {
                Ok(n @ 1..) => buf.extend_from_slice(&more[..n]),
                _ => break,
            }
        }
        // A paste waits for its end however slowly the rest comes, so none of its lines is sent on its own.
        if picker::pasting(&buf) {
            continue;
        }
        let mut steer = STEER.lock().unwrap();
        for key in picker::keys(&std::mem::take(&mut buf)) {
            steer.key(key);
        }
        drop(steer);
        draw();
    }
}

/// Lets the REPL's turns read what is typed while they run.
pub fn steerable() {
    STEERABLE.store(true, Ordering::Relaxed);
}

/// What the user sent to steer the turn since the last request, as one message.
pub fn steers() -> Option<String> {
    let taken = STEER.lock().unwrap().take();
    if taken.is_some() {
        draw();
    }
    taken
}

/// Shows a message the user sent during the turn where the model gets it.
pub fn steered(text: &str) {
    settle();
    println!("{} {}", paint("36", "›"), line(text));
}

/// What the REPL runs next, before it reads a line: what was sent during the last turn and no request took, the lines
/// before the first command as one task, or that command.
pub fn queued() -> Option<String> {
    let mut steer = STEER.lock().unwrap();
    steer.take().or_else(|| (!steer.sent.is_empty()).then(|| steer.sent.remove(0)))
}

/// What was being typed when the turn ended, for the next prompt to start with.
pub fn typed() -> String {
    std::mem::take(&mut STEER.lock().unwrap().typing)
}

/// After a Ctrl-C nothing sent runs on its own: it all goes back to the prompt, to send again or change.
pub fn unsend() {
    let mut steer = STEER.lock().unwrap();
    let typing = std::mem::take(&mut steer.typing);
    let mut lines = std::mem::take(&mut steer.sent);
    lines.extend((!typing.is_empty()).then_some(typing));
    steer.typing = lines.join(" ");
}

/// Marks the picker as up, so a signal that ends low shows the cursor again.
pub fn picking(up: bool) {
    PICKING.store(up, Ordering::Relaxed);
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
    let reset: &[u8] = match (BARRED.load(Ordering::Relaxed), STEERING.load(Ordering::Relaxed)) {
        (true, true) => b"\x1b[?2004l\x1b7\x1b[999;1H\x1b[2K\x1b[1A\x1b[2K\x1b[r\x1b8\n",
        (true, false) => b"\x1b7\x1b[999;1H\x1b[2K\x1b[r\x1b8\n",
        _ => b"",
    };
    let cursor: &[u8] = if PICKING.load(Ordering::Relaxed) { b"\x1b[?25h\n" } else { b"" };
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
    let _ = rustix::io::write(stdout(), cursor);
    unsafe { libc::_exit(128 + signal) };
}

/// Keeps the bottom row for the bar, and the one above it for steering, out of the scrolling output.
fn reserve(rows: u16, steering: bool) {
    let n = 1 + u16::from(steering);
    print!("{}\x1b[{n}A\x1b7\x1b[1;{}r\x1b8", "\n".repeat(n.into()), rows.saturating_sub(n));
}

fn resize() {
    let Some((rows, cols)) = size() else { return };
    let mut bar = BAR.lock().unwrap();
    if let Some(bar) = bar.as_mut().filter(|b| (b.rows, b.cols) != (rows, cols)) {
        (bar.rows, bar.cols) = (rows, cols);
        reserve(rows, bar.steering);
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
    let above = match bar.steering {
        true => format!("\x1b[{};1H\x1b[2K{}", bar.rows - 1, STEER.lock().unwrap().row(bar.cols.into())),
        false => String::new(),
    };
    print!("\x1b7{above}\x1b[{};1H\x1b[2K\x1b[7m{line}\x1b[0m\x1b8", bar.rows);
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
    group: Option<Group>,
    /// What the model said before its next run, and whether that names a file or symbol.
    intent: Option<(String, bool)>,
    running: Option<(Instant, String, bool)>,
}

/// Runs in a row that only read, shown as one row: under the first thing the model said it was after that names a
/// file or symbol, when one did.
struct Group {
    runs: usize,
    files: Vec<String>,
    label: Option<String>,
    began: Instant,
}

static LIVE: Mutex<Live> =
    Mutex::new(Live { drawn: 0, at: None, rows: Vec::new(), group: None, intent: None, running: None });
static PLAN: Mutex<(Vec<String>, Vec<u64>)> = Mutex::new((Vec::new(), Vec::new()));
/// The repository's root, which the commands shown leave out of their paths.
static ROOT: Mutex<String> = Mutex::new(String::new());
const LIVE_LINES: usize = 3;
/// A group of runs that only read goes to the scrollback when it has this many runs or has run this long, so a long
/// stretch of reading still leaves a row a minute.
const GROUP_RUNS: usize = 10;
const GROUP_TIME: Duration = Duration::from_secs(60);
const INSPECT: &str = "cat|head|tail|sed|nl|wc|ls|tree|find|grep|rg|file|stat|cd|pwd|echo|diff|sort|uniq|cut|awk|jq|\
                       which|du|command -v|git log|git show|git diff|git status|git grep|git blame|git ls-files|\
                       git ls-tree|git rev-parse|git cat-file";

pub fn rooted(root: &std::path::Path) {
    *ROOT.lock().unwrap() = root.to_string_lossy().into_owned();
}

/// The commands shell text runs, split where an operator outside quotes ends one; None when it writes to a file or
/// substitutes a command, which no read needs. A redirect to /dev/null or another descriptor is left out of them.
fn stages(cmd: &str) -> Option<Vec<String>> {
    let (mut stages, mut stage, mut quote) = (vec![], String::new(), None);
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), _) if c == q => {
                quote = None;
                stage.push(c);
            }
            (Some('\''), _) => stage.push(c),
            (_, '`') => return None,
            (_, '$') if chars.peek() == Some(&'(') => return None,
            (_, '\\') => {
                stage.push(c);
                stage.extend(chars.next());
            }
            (Some(_), _) => stage.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                stage.push(c);
            }
            (None, '|' | '&' | ';' | '\n') => {
                stages.push(std::mem::take(&mut stage));
                if c != ';' && c != '\n' {
                    chars.next_if(|n| matches!(n, '|' | '&'));
                }
            }
            (None, '<') if chars.peek() == Some(&'(') => return None,
            (None, '>') => {
                chars.next_if(|n| matches!(n, '>' | '|'));
                let dup = chars.next_if_eq(&'&').is_some();
                while chars.next_if(|n| *n == ' ' || *n == '\t').is_some() {}
                let mut target = String::new();
                while let Some(n) = chars.next_if(|n| !n.is_whitespace() && !"|&;<>".contains(*n)) {
                    target.push(n);
                }
                let descriptor =
                    dup && (target == "-" || !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()));
                if target != "/dev/null" && !descriptor {
                    return None;
                }
            }
            (None, _) => stage.push(c),
        }
    }
    stages.push(stage);
    Some(stages.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

/// Whether a command only reads: each of its commands is one that inspects, through xargs or not, and none writes.
fn inspects(cmd: &str) -> bool {
    let read = |stage: &String| {
        let words: Vec<&str> = stage.split_whitespace().collect();
        let words = match words.split_first() {
            Some((&"xargs", rest)) => &rest[rest.iter().take_while(|w| w.starts_with('-')).count()..],
            _ => &words[..],
        };
        let line = words.join(" ");
        let named =
            INSPECT.split('|').find(|i| line.strip_prefix(i).is_some_and(|r| r.is_empty() || r.starts_with(' ')));
        match named {
            None => false,
            Some("sed") => !words.iter().any(|w| {
                w.starts_with("--in-place") || w.starts_with('-') && !w.starts_with("--") && w[1..].contains('i')
            }),
            Some("find") => !words.iter().any(|w| matches!(*w, "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir")),
            Some(_) => true,
        }
    };
    stages(cmd).is_some_and(|stages| stages.iter().all(read))
}

/// A command as the user reads it: paths in the repository relative to it, the temporary directory as $TMPDIR, the
/// home as ~, and a program named by its full path by its name alone.
fn short(cmd: &str) -> String {
    let temp = std::env::temp_dir().to_string_lossy().trim_end_matches('/').to_string();
    let home = std::env::var("HOME").unwrap_or_default();
    let mut text = cmd.to_string();
    for (long, short) in [
        (ROOT.lock().unwrap().clone(), ""),
        (format!("/private{temp}"), "$TMPDIR/"),
        (temp, "$TMPDIR/"),
        (home.trim_end_matches('/').to_string(), "~/"),
    ] {
        if !long.is_empty() {
            text = text.replace(&format!("{long}/"), short);
        }
    }
    let program = text.split_whitespace().next().unwrap_or_default();
    match program.rsplit_once('/') {
        Some((_, name)) if program.starts_with('/') && !name.is_empty() => {
            format!("{name}{}", &text.trim_start()[program.len()..])
        }
        _ => text,
    }
}

/// How a command that failed ended: its exit status, or the signal that stopped it (a negative status).
fn ended(code: i64) -> String {
    match code {
        -9 => "killed".into(),
        -15 => "terminated".into(),
        -2 => "interrupted".into(),
        code if code < 0 => format!("signal {}", -code),
        code => format!("exit {code}"),
    }
}

fn redraw(live: &mut Live) {
    let Some(cols) = BAR.lock().unwrap().as_ref().map(|b| b.cols as usize) else { return };
    let group = live.group.as_ref().map(grouped).unwrap_or_default();
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
    settled(&mut LIVE.lock().unwrap());
}

fn settled(live: &mut Live) {
    erase(live);
    live.rows.clear();
    if let Some(group) = live.group.take() {
        for (style, text) in grouped(&group) {
            println!("  {}", paint(style, &text));
        }
    }
}

/// A group's rows: what the model said it was after, when it named something, over the files read and the time taken.
fn grouped(group: &Group) -> Vec<(&'static str, String)> {
    let Group { runs, files, label, began } = group;
    let secs = began.elapsed().as_secs();
    let took = if secs > 0 { format!(" · {}", minutes(secs)) } else { String::new() };
    let count = if *runs > 1 { format!(" · {runs} runs") } else { String::new() };
    let lead = if label.is_some() { "  " } else { "◦ " };
    let read = if files.is_empty() {
        format!("{lead}{runs} run{}{took}", if *runs == 1 { "" } else { "s" })
    } else {
        let room = width().saturating_sub(18 + count.len() + took.len()).max(20);
        let (mut list, mut shown) = (String::new(), 0);
        for file in files {
            if shown > 0 && list.width() + 2 + file.width() > room {
                break;
            }
            list += &format!("{}{file}", if shown > 0 { ", " } else { "" });
            shown += 1;
        }
        let rest = if shown < files.len() { format!(" +{}", files.len() - shown) } else { String::new() };
        format!("{lead}read {list}{rest}{count}{took}")
    };
    match label {
        Some(label) => vec![("0", format!("◦ {}", flat(label, width().saturating_sub(6).max(30)))), ("2", read)],
        None => vec![("2", read)],
    }
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

/// What the model is after in its next run: the last sentence it said, or thought, that names a file or symbol, and
/// otherwise its last sentence. Reasoning is often notes ("Search.", "inspect"), which a sentence that names
/// something mostly isn't: in a 200-run session it gave 171 runs such a sentence.
pub fn intent(said: &str, thinking: &str) {
    let text = if said.trim().is_empty() { thinking } else { said };
    let sentences = sentences(text);
    let named = sentences.iter().rev().find(|s| s.chars().count() >= 25 && names(s));
    let intent = match (named, sentences.as_slice()) {
        (Some(named), _) => Some((named.to_string(), true)),
        (None, []) => None,
        (None, [.., before, last]) if last.chars().count() < 25 => Some((format!("{before} {last}"), false)),
        (None, [.., last]) => Some((last.to_string(), false)),
    };
    LIVE.lock().unwrap().intent = intent.map(|(text, named)| {
        (text.replace("**", "").replace('`', "").trim_start_matches(['-', '*', '#', ' ']).to_string(), named)
    });
}

/// Whether a sentence names a file or a symbol: `code`, a path, a file name with an extension, snake_case, a::b, f().
fn names(sentence: &str) -> bool {
    sentence.contains('`')
        || sentence.split_whitespace().any(|word| {
            let word = word.trim_matches(|c: char| !c.is_alphanumeric() && !"_/.:()".contains(c));
            let word = word.trim_end_matches(['.', ':']);
            let extension = word.rsplit_once('.').is_some_and(|(stem, ext)| {
                stem.chars().filter(|c| c.is_alphanumeric()).count() >= 2
                    && (1..=4).contains(&ext.len())
                    && ext.bytes().all(|b| b.is_ascii_alphanumeric())
            });
            let alpha = word.chars().any(char::is_alphabetic);
            alpha && (extension || word.contains("::") || word.ends_with("()") || word.trim_matches('_').contains('_'))
                || alpha && word.contains('/') && !word.starts_with("//")
        })
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
    let (label, named) = live.intent.take().unwrap_or_else(|| (first.to_string(), false));
    let label = flat(&label, 300);
    live.rows = vec![("2", format!("▸ {label}"))];
    live.running = Some((Instant::now(), label, named));
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
    let (started, label, named) = match live.running.take() {
        Some((started, label, named)) => (Some(started), Some(label), named),
        None => (None, None, false),
    };
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
        let began = started.unwrap_or_else(Instant::now);
        let group = live.group.get_or_insert_with(|| Group { runs: 0, files: vec![], label: None, began });
        group.runs += 1;
        if group.label.is_none() && named {
            group.label = label;
        }
        for read in reads {
            if !group.files.contains(&read) {
                group.files.push(read);
            }
        }
        if group.runs >= GROUP_RUNS || group.began.elapsed() >= GROUP_TIME {
            settled(&mut live);
        } else {
            redraw(&mut live);
        }
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
    // A command run again and again in a row, as a loop that polls runs it, is one row with how many times.
    let mut repeated: Vec<(&str, &Value, usize)> = vec![];
    for (cmd, exit) in cmds {
        match repeated.last_mut() {
            Some((last, last_exit, n)) if *last == cmd && *last_exit == exit => *n += 1,
            _ => repeated.push((cmd, exit, 1)),
        }
    }
    for (i, (cmd, exit, n)) in repeated.iter().enumerate() {
        if i == 3 {
            let more: usize = repeated[3..].iter().map(|(_, _, n)| n).sum();
            rows.push(paint("2", &format!("  +{more} more command{}", if more == 1 { "" } else { "s" })));
            break;
        }
        let times = if *n > 1 { format!(" ×{n}") } else { String::new() };
        let cmd = flat(&short(cmd), room.saturating_sub(16 + times.len()));
        rows.push(match exit.as_i64() {
            Some(0) => format!("{} {cmd}  {}{times}", paint("36", "$"), paint("32", "✓")),
            Some(code) => paint("2", &format!("$ {cmd}  ✗ {}{times}", ended(code))),
            None => paint("2", &format!("$ {cmd}  ✗ {}{times}", flat(exit.as_str().unwrap_or("failed"), 40))),
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

#[cfg(test)]
mod tests {
    use super::{Steer, ended, ending, inspects, names, short};
    use crate::picker::{Key, keys};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn a_command_that_only_reads_is_told_by_its_commands_outside_quotes() {
        for read in [
            r"grep -nE '^(pub )?fn |^impl ' src/agent.rs",
            r"grep -n 'def _read\|MAX_OUT' src/worker.py | head -30",
            r"grep -n -i 'orphan\|capture' review/PLAN.md 2>/dev/null | head -80",
            "git cat-file -t ed68baf 2>&1; git log --oneline ed68baf..HEAD 2>&1 | head -60",
            "git ls-files | xargs wc -l | sort -n | tail -30",
            "cd src && grep -c x *.rs >/dev/null || echo none",
            r#"grep "a;b|c" f.rs"#,
            "command -v lsof",
        ] {
            assert!(inspects(read), "{read}");
        }
        for runs in [
            "cat a > b",
            "grep x f | tee out",
            "echo $(rm -rf x)",
            "echo `make`",
            "cat x\nrm -rf y",
            "grep x f; make",
            "sed -i 's/a/b/' f",
            "sed -ni 's/a/b/p' f",
            "find . -name '*.pyc' -delete",
            "find . -exec rm {} +",
            "diff <(make a) b",
            "lsof -p 1",
            "/opt/homebrew/bin/python3.14 src/worker.py",
        ] {
            assert!(!inspects(runs), "{runs}");
        }
    }

    #[test]
    fn a_command_shows_short_paths_and_how_it_ended() {
        super::rooted(std::path::Path::new("/r/low"));
        let temp = std::env::temp_dir();
        let temp = temp.to_string_lossy();
        let cmd =
            format!("/opt/py/bin/python3.14 /r/low/src/worker.py; rm -rf {}/low-x /r/lowx", temp.trim_end_matches('/'));
        assert_eq!(short(&cmd), "python3.14 src/worker.py; rm -rf $TMPDIR/low-x /r/lowx");
        assert_eq!(short("git status"), "git status");
        assert_eq!((ended(-9), ended(-6), ended(1)), ("killed".into(), "signal 6".into(), "exit 1".into()));
    }

    #[test]
    fn a_sentence_names_something_when_it_has_a_path_a_file_or_a_symbol() {
        for named in [
            "Let's inspect llm.rs around IDLE/retries",
            "Potential bug in `Session` lock/resume",
            "check whether compact_at is learned",
            "does Session::resume load it all",
            "where finish() redacts",
        ] {
            assert!(names(named), "{named}");
        }
        for vague in
            ["Search.", "Let's do that now, e.g. read the rest.", "Read key areas of the worker", "1.5 seconds"]
        {
            assert!(!names(vague), "{vague}");
        }
    }

    fn typed(bytes: &[u8]) -> Steer {
        let mut steer = Steer { typing: String::new(), sent: vec![] };
        for key in keys(bytes) {
            steer.key(key);
        }
        steer
    }

    #[test]
    fn typing_sends_lines_and_takes_one_back() {
        let mut steer = typed(b"use tabsx\x7f\r  \r\x15gone\x15/models\rand tests\rhalf");
        assert_eq!(
            (&steer.sent[..], steer.typing.as_str()),
            (&["use tabs", "/models", "and tests"].map(String::from)[..], "half")
        );
        steer.typing.clear();
        steer.key(Key::Up);
        assert_eq!((steer.sent.len(), steer.typing.as_str()), (2, "and tests"));
        // Up while typing changes nothing.
        steer.key(Key::Up);
        assert_eq!((steer.sent.len(), steer.typing.as_str()), (2, "and tests"));
    }

    #[test]
    fn a_command_waits_for_the_lines_before_it() {
        let mut steer = typed(b"one\r/src/main.rs too\r/models\rthree\r");
        assert_eq!(steer.take().as_deref(), Some("one\n/src/main.rs too"));
        assert_eq!(steer.take(), None);
        assert_eq!(steer.sent, ["/models", "three"]);
    }

    #[test]
    fn a_paste_is_one_message_however_many_lines() {
        let mut steer = typed(b"\x1b[200~fix this:\r\n\tfn f() {\r\n\r\n/models\x1b[201~");
        let row = steer.row(60);
        assert!(row.ends_with("fix this:⏎ fn f() {⏎⏎/models▏") && !row.contains(['\n', '\t']), "{row:?}");
        steer.key(Key::Enter);
        assert_eq!(steer.take().as_deref(), Some("fix this:\n\tfn f() {\n\n/models"));
        // Only the blank lines around it go: the first line keeps its indentation, as the others do.
        let mut steer = typed(b"\x1b[200~\r\n  \r\n    def f():\r\n        pass\r\n\r\n\x1b[201~\r");
        assert_eq!(steer.take().as_deref(), Some("    def f():\n        pass"));
    }

    #[test]
    fn the_row_fits_the_terminal() {
        let plain = |s: String| {
            let mut parts = s.split('\x1b');
            let first = parts.next().unwrap_or_default().to_string();
            first + &parts.map(|p| p.split_once('m').map_or(p, |(_, rest)| rest)).collect::<String>()
        };
        for bytes in [&b""[..], b"queued\r", b"/models\r", "a long line with 漢字 in it ".repeat(9).as_bytes()] {
            let steer = typed(bytes);
            for cols in [20, 41, 100] {
                let row = plain(steer.row(cols));
                assert!(row.width() < cols && row.starts_with("› "), "{cols} {row:?}");
            }
        }
        // What is typed shows its end, where the cursor is.
        assert!(plain(typed(b"first words, then the last ones").row(20)).ends_with("the last ones▏"));
        assert_eq!(ending("ab漢字", 3), "字");
    }
}
