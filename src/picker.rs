//! /models: one panel under the prompt to pick the model (↑↓, typing filters) and the reasoning effort (←→). It draws
//! inline, so the scrollback stays, and erases itself when it closes.
use crate::agent::EFFORTS;
use crate::ui;
use anyhow::Result;
use rustix::stdio::stdin;
use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcsetattr};
use std::collections::BTreeMap;
use std::io::Write;
use unicode_width::UnicodeWidthStr;

/// Model rows the panel shows at most; a short terminal gets fewer.
const ROWS: usize = 10;

pub struct Row {
    pub provider: String,
    pub model: String,
    pub profiled: bool,
    pub current: bool,
}

#[derive(Debug, PartialEq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Backspace,
    Clear,
    Char(char),
}

#[derive(Debug, PartialEq)]
pub enum Target {
    /// A row of the list, by its index.
    Listed(usize),
    /// A model id no row matched, typed as the filter.
    Typed(String),
}

#[derive(Debug, PartialEq)]
pub struct Picked {
    pub target: Target,
    /// None is the model profile's reasoning_effort, or none at all.
    pub effort: Option<&'static str>,
}

pub struct State {
    rows: Vec<Row>,
    /// Each model's reasoning_effort from its profile, for the default's label.
    defaults: BTreeMap<String, String>,
    filter: String,
    shown: Vec<usize>,
    cursor: usize,
    offset: usize,
    /// 0 is the default, then EFFORTS in order.
    effort: usize,
    height: usize,
}

impl State {
    pub fn new(rows: Vec<Row>, defaults: BTreeMap<String, String>, filter: &str, effort: Option<&str>) -> Self {
        let effort = effort.and_then(|e| EFFORTS.iter().position(|x| *x == e)).map_or(0, |i| i + 1);
        let mut state =
            Self { rows, defaults, filter: filter.into(), shown: vec![], cursor: 0, offset: 0, effort, height: ROWS };
        state.refilter();
        state
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    fn refilter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.shown = (0..self.rows.len()).filter(|&i| self.rows[i].model.to_lowercase().contains(&needle)).collect();
        let current = self.shown.iter().position(|&i| self.rows[i].current);
        self.cursor = if self.filter.is_empty() { current.unwrap_or(0) } else { 0 };
        self.offset = 0;
        self.scroll();
    }

    /// The typed filter, when it names no listed model exactly: the last row offers to try it.
    fn typed(&self) -> Option<&str> {
        let typed = self.filter.trim();
        let named = |r: &Row| r.model == typed || format!("{}:{}", r.provider, r.model) == typed;
        (!typed.is_empty() && !self.rows.iter().any(named)).then_some(typed)
    }

    fn len(&self) -> usize {
        self.shown.len() + usize::from(self.typed().is_some())
    }

    fn scroll(&mut self) {
        self.cursor = self.cursor.min(self.len().saturating_sub(1));
        if self.cursor < self.offset {
            self.offset = self.cursor;
        }
        if self.cursor >= self.offset + self.height {
            self.offset = self.cursor + 1 - self.height;
        }
        self.offset = self.offset.min(self.len().saturating_sub(self.height));
    }

    fn resize(&mut self, rows: usize) {
        self.height = ROWS.min(rows.saturating_sub(6)).max(1);
        self.scroll();
    }

    /// The model under the cursor, if it is a listed one.
    fn highlighted(&self) -> Option<&str> {
        match self.shown.get(self.cursor) {
            Some(&i) => Some(&self.rows[i].model),
            None => self.typed(),
        }
    }

    /// Takes a key: Some(true) applies the choice, Some(false) cancels.
    pub fn key(&mut self, key: Key) -> Option<bool> {
        let last = self.len().saturating_sub(1);
        match key {
            Key::Up => self.cursor = self.cursor.saturating_sub(1),
            Key::Down => self.cursor = (self.cursor + 1).min(last),
            Key::PageUp => self.cursor = self.cursor.saturating_sub(self.height),
            Key::PageDown => self.cursor = (self.cursor + self.height).min(last),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = last,
            Key::Left => self.effort = self.effort.saturating_sub(1),
            Key::Right => self.effort = (self.effort + 1).min(EFFORTS.len()),
            Key::Enter => return (self.len() > 0).then_some(true),
            Key::Esc => return Some(false),
            Key::Backspace | Key::Clear | Key::Char(_) => {
                match key {
                    Key::Backspace => drop(self.filter.pop()),
                    Key::Clear => self.filter.clear(),
                    Key::Char(c) => self.filter.push(c),
                    _ => {}
                }
                self.refilter();
            }
        }
        self.scroll();
        None
    }

    pub fn picked(&self) -> Option<Picked> {
        let target = match self.shown.get(self.cursor) {
            Some(&i) => Target::Listed(i),
            None => Target::Typed(self.typed()?.to_string()),
        };
        Some(Picked { target, effort: self.effort.checked_sub(1).map(|i| EFFORTS[i]) })
    }
}

/// One line of styled pieces, cut to `room` columns before it is painted, so it never wraps.
fn line(pieces: &[(&str, String)], room: usize) -> String {
    let (mut out, mut left) = (String::new(), room);
    for (style, text) in pieces {
        let text = ui::fit(text, left);
        left -= text.width();
        out += &if style.is_empty() || text.is_empty() { text.to_string() } else { ui::paint(style, text) };
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let text = ui::fit(text, width);
    format!("{text}{}", " ".repeat(width - text.width()))
}

pub fn render(state: &State, cols: usize) -> Vec<String> {
    let room = cols.saturating_sub(1);
    let mut lines = vec![];
    let count = format!("{} of {}", state.shown.len(), state.rows.len());
    let filter = ui::printable(&state.filter);
    let head = if filter.is_empty() { "  type to filter".to_string() } else { format!("  filter {filter}▏") };
    let gap = room.saturating_sub(head.width() + count.width()).max(2);
    lines.push(line(&[("2", head), ("", " ".repeat(gap)), ("2", count)], room));
    let models = state.rows.iter().map(|r| ui::printable(&r.model).width()).max().unwrap_or(0);
    let width = models.min(room / 2).max(8);
    for at in state.offset..(state.offset + state.height).min(state.len()) {
        let on = at == state.cursor;
        let marker = ("36", if on { "  ❯ " } else { "    " }.to_string());
        let bold = if on { "1" } else { "" };
        let pieces = match state.shown.get(at) {
            Some(&i) => {
                let row = &state.rows[i];
                vec![
                    marker,
                    (bold, pad(&ui::printable(&row.model), width)),
                    ("2", format!("  {}", ui::printable(&row.provider))),
                    ("2", if row.profiled { "  profile" } else { "" }.into()),
                    ("32", if row.current { "  ● current" } else { "" }.into()),
                ]
            }
            None => vec![marker, (bold, format!("+ try \"{filter}\"")), ("2", "  not listed".into())],
        };
        lines.push(line(&pieces, room));
    }
    let below = state.len().saturating_sub(state.offset + state.height);
    if state.offset > 0 || below > 0 {
        let above = if state.offset > 0 { format!("↑ {} more  ", state.offset) } else { String::new() };
        let below = if below > 0 { format!("↓ {below} more") } else { String::new() };
        lines.push(line(&[("2", format!("    {above}{below}"))], room));
    }
    if state.len() == 0 {
        lines.push(line(&[("2", "    no model matches".into())], room));
    }
    lines.push(String::new());
    let default = match state.highlighted().and_then(|m| state.defaults.get(m)) {
        Some(effort) => format!("default ({})", ui::printable(effort)),
        None => "default".into(),
    };
    let mut pieces = vec![("2", "  effort  ".to_string())];
    for (i, label) in std::iter::once(default.as_str()).chain(EFFORTS.iter().copied()).enumerate() {
        pieces.push(match i == state.effort {
            true if ui::styled() => ("7", format!(" {label} ")),
            true => ("", format!("[{label}]")),
            false => ("", format!(" {label} ")),
        });
        pieces.push(("", " ".into()));
    }
    lines.push(line(&pieces, room));
    let help = "  ↑↓ model · ←→ effort · type to filter · enter apply · esc cancel";
    lines.push(line(&[("2", help.into())], room));
    lines
}

/// The keys in what one read of the terminal brought. Escape sequences low has no use for are skipped.
pub fn keys(bytes: &[u8]) -> Vec<Key> {
    let mut out = vec![];
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        match b {
            0x1b => match bytes.get(i) {
                Some(b'[' | b'O') => {
                    let start = i + 1;
                    let Some(end) = bytes[start..].iter().position(|b| (0x40..=0x7e).contains(b)) else { break };
                    let (params, last) = (&bytes[start..start + end], bytes[start + end]);
                    i = start + end + 1;
                    out.extend(match (last, params) {
                        (b'A', _) => Some(Key::Up),
                        (b'B', _) => Some(Key::Down),
                        (b'C', _) => Some(Key::Right),
                        (b'D', _) => Some(Key::Left),
                        (b'H', _) | (b'~', b"1" | b"7") => Some(Key::Home),
                        (b'F', _) | (b'~', b"4" | b"8") => Some(Key::End),
                        (b'~', b"5") => Some(Key::PageUp),
                        (b'~', b"6") => Some(Key::PageDown),
                        _ => None,
                    });
                }
                _ => out.push(Key::Esc),
            },
            b'\r' | b'\n' => out.push(Key::Enter),
            0x7f | 0x08 => out.push(Key::Backspace),
            0x03 => out.push(Key::Esc),
            0x15 => out.push(Key::Clear),
            0x10 => out.push(Key::Up),
            0x0e => out.push(Key::Down),
            ..0x20 => {}
            _ => {
                let len = match b {
                    0xf0.. => 4,
                    0xe0.. => 3,
                    0xc0.. => 2,
                    _ => 1,
                };
                let end = (i - 1 + len).min(bytes.len());
                if let Some(c) = std::str::from_utf8(&bytes[i - 1..end]).ok().and_then(|s| s.chars().next()) {
                    out.push(Key::Char(c));
                }
                i = end;
            }
        }
    }
    out
}

/// Whether the bytes end inside an escape sequence, so the rest is still on its way.
fn incomplete(bytes: &[u8]) -> bool {
    let Some(at) = bytes.iter().rposition(|&b| b == 0x1b) else { return false };
    match &bytes[at + 1..] {
        [] => true,
        [b'[', rest @ ..] => !rest.iter().any(|b| (0x40..=0x7e).contains(b)),
        [b'O'] => true,
        _ => false,
    }
}

/// What the picker does on Enter: switch to the model (which brings its profile, effort included), then set the
/// effort when the one picked is not what that leaves.
#[derive(Debug, PartialEq)]
pub enum Step {
    Switch,
    Effort(&'static str),
}

/// `now` and `to` are (provider, model); `effort_now` is what the session sends, `profile` what `to`'s profile sets.
pub fn steps(
    now: (&str, &str),
    effort_now: Option<&str>,
    to: (&str, &str),
    profile: Option<&str>,
    effort: Option<&'static str>,
) -> Vec<Step> {
    match effort {
        _ if now != to => {
            let after = effort.filter(|e| profile != Some(*e)).map(Step::Effort);
            std::iter::once(Step::Switch).chain(after).collect()
        }
        Some(e) if effort_now != Some(e) => vec![Step::Effort(e)],
        None if effort_now != profile => vec![Step::Switch],
        _ => vec![],
    }
}

/// The terminal with input unbuffered and unechoed, and Ctrl-C as a key; output keeps its processing, so \n still
/// returns the carriage. Dropping it puts everything back.
struct Raw(Termios);

impl Raw {
    fn start() -> Result<Self> {
        let saved = tcgetattr(stdin())?;
        let mut raw = saved.clone();
        raw.local_modes.remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG | LocalModes::IEXTEN);
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        tcsetattr(stdin(), OptionalActions::Now, &raw)?;
        ui::picking(true);
        print!("\x1b[?25l");
        let _ = std::io::stdout().flush();
        Ok(Self(saved))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = tcsetattr(stdin(), OptionalActions::Now, &self.0);
        print!("\x1b[?25h");
        let _ = std::io::stdout().flush();
        ui::picking(false);
    }
}

/// Whether stdin has input within `ms` milliseconds.
fn ready(ms: i32) -> bool {
    let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut fd, 1, ms) > 0 }
}

/// Moves to the panel's top, clears below and draws `lines`; returns how many lines it drew.
fn draw(drawn: usize, lines: &[String]) -> usize {
    let mut out = if drawn > 0 { format!("\x1b[{drawn}A") } else { String::new() };
    out += "\r\x1b[J";
    for line in lines {
        out += line;
        out += "\n";
    }
    print!("{out}");
    let _ = std::io::stdout().flush();
    lines.len()
}

/// Shows the panel until Enter (the choice) or Esc and Ctrl-C (None).
pub fn choose(state: &mut State) -> Result<Option<Picked>> {
    let _raw = Raw::start()?;
    let (mut drawn, mut size, mut dirty) = (0, None, true);
    let done = loop {
        let now = ui::size();
        if dirty || now != size {
            let (rows, cols) = now.unwrap_or((24, 80));
            state.resize(rows as usize);
            drawn = draw(drawn, &render(state, cols as usize));
            (size, dirty) = (now, false);
        }
        // Waking now and then notices a resized terminal.
        if !ready(250) {
            continue;
        }
        let mut buf = vec![0; 256];
        let n = match rustix::io::read(stdin(), &mut buf[..]) {
            Ok(0) => break false,
            Ok(n) => n,
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => Err(e)?,
        };
        buf.truncate(n);
        // A lone Esc is the key; the rest of a sequence comes right behind its first byte.
        while incomplete(&buf) && ready(50) {
            let mut more = [0; 64];
            match rustix::io::read(stdin(), &mut more[..]) {
                Ok(n @ 1..) => buf.extend_from_slice(&more[..n]),
                _ => break,
            }
        }
        if let Some(apply) = keys(&buf).into_iter().find_map(|key| state.key(key)) {
            break apply;
        }
        dirty = true;
    };
    print!("\x1b[{drawn}A\r\x1b[J");
    let _ = std::io::stdout().flush();
    Ok(if done { state.picked() } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Row> {
        let row = |provider: &str, model: &str, current| Row {
            provider: provider.into(),
            model: model.into(),
            profiled: model.contains("flash"),
            current,
        };
        vec![
            row("opencode", "deepseek-v4.1-flash", false),
            row("opencode", "deepseek-v4.1-pro", true),
            row("zro", "zro/deepseek-v4.1-flash", false),
            row("zro", "glm-5", false),
        ]
    }

    /// A rendered line without its colors.
    fn plain(line: &str) -> String {
        let mut out = String::new();
        let mut parts = line.split('\x1b');
        out += parts.next().unwrap_or_default();
        for part in parts {
            out += part.strip_prefix('[').and_then(|p| p.split_once('m')).map_or(part, |(_, rest)| rest);
        }
        out
    }

    fn state(filter: &str) -> State {
        let defaults = BTreeMap::from([("deepseek-v4.1-flash".to_string(), "low".to_string())]);
        State::new(rows(), defaults, filter, None)
    }

    #[test]
    fn keys_read_both_arrow_forms_text_and_controls() {
        assert_eq!(keys(b"\x1b[A\x1bOB\x1b[C\x1bOD"), [Key::Up, Key::Down, Key::Right, Key::Left]);
        assert_eq!(keys(b"\x1b[5~\x1b[6~\x1b[H\x1b[4~"), [Key::PageUp, Key::PageDown, Key::Home, Key::End]);
        assert_eq!(keys(b"\x1b"), [Key::Esc]);
        assert_eq!(keys(b"\x03"), [Key::Esc]);
        assert_eq!(keys(b"g\xc3\xa9\x7f\n"), [Key::Char('g'), Key::Char('é'), Key::Backspace, Key::Enter]);
        assert_eq!(keys(b"\x1b[200~x\x1b[201~"), [Key::Char('x')]);
        assert_eq!(keys(b"\x15\x01"), [Key::Clear]);
    }

    #[test]
    fn a_split_sequence_waits_for_the_rest() {
        assert!(incomplete(b"\x1b"));
        assert!(incomplete(b"a\x1b["));
        assert!(incomplete(b"\x1b[1"));
        assert!(incomplete(b"\x1bO"));
        assert!(!incomplete(b"\x1b[A"));
        assert!(!incomplete(b"abc"));
    }

    #[test]
    fn starts_on_the_current_model_and_the_filter_narrows() {
        let mut s = state("");
        assert_eq!(s.picked().unwrap().target, Target::Listed(1));
        for c in "FLASH".chars() {
            s.key(Key::Char(c));
        }
        assert_eq!(s.shown, [0, 2]);
        s.key(Key::Down);
        assert_eq!(s.picked().unwrap().target, Target::Listed(2));
        s.key(Key::Down);
        assert_eq!(s.picked().unwrap().target, Target::Typed("FLASH".into()));
        s.key(Key::Clear);
        assert_eq!(s.shown.len(), 4);
        assert_eq!(s.picked().unwrap().target, Target::Listed(1));
    }

    #[test]
    fn an_unlisted_name_can_be_tried() {
        let mut s = state("qwen3:8b");
        assert!(s.shown.is_empty());
        assert_eq!(s.key(Key::Enter), Some(true));
        assert_eq!(s.picked().unwrap().target, Target::Typed("qwen3:8b".into()));
        assert_eq!(state("glm-5").len(), 1);
        assert_eq!(state("zro:glm-5").typed(), None);
    }

    #[test]
    fn effort_stops_at_both_ends() {
        let mut s = State::new(rows(), BTreeMap::new(), "", Some("high"));
        assert_eq!(s.picked().unwrap().effort, Some("high"));
        for _ in 0..9 {
            s.key(Key::Right);
        }
        assert_eq!(s.picked().unwrap().effort, Some("max"));
        for _ in 0..9 {
            s.key(Key::Left);
        }
        assert_eq!(s.picked().unwrap().effort, None);
        assert_eq!(s.key(Key::Esc), Some(false));
    }

    #[test]
    fn the_window_follows_the_cursor() {
        let mut s = state("");
        s.resize(8);
        assert_eq!(s.height, 2);
        s.key(Key::End);
        assert_eq!((s.cursor, s.offset), (3, 2));
        s.key(Key::Home);
        assert_eq!((s.cursor, s.offset), (0, 0));
        let lines = render(&s, 80);
        assert!(lines.iter().any(|l| l.contains("↓ 2 more")));
    }

    #[test]
    fn no_line_is_wider_than_the_terminal() {
        let mut list = rows();
        list.push(Row {
            provider: "p".into(),
            model: "模型\x1b[31m-very-long-model-name".into(),
            profiled: true,
            current: false,
        });
        let s = State::new(list, BTreeMap::new(), "", None);
        for cols in [20, 40, 80] {
            for l in render(&s, cols).iter().map(|l| plain(l)) {
                assert!(l.width() < cols, "{l:?} is wider than {cols}");
                assert!(!l.contains('\x1b'));
            }
        }
        assert!(render(&state(""), 80).iter().any(|l| l.contains("● current")));
    }

    #[test]
    fn the_default_label_follows_the_highlighted_model() {
        let mut s = state("");
        assert!(render(&s, 80).iter().any(|l| l.contains("default") && !l.contains("default (")));
        s.key(Key::Up);
        assert!(render(&s, 80).iter().any(|l| l.contains("default (low)")));
    }

    #[test]
    fn steps_switch_then_set_only_what_changed() {
        let (a, b) = (("p", "a"), ("p", "b"));
        assert_eq!(steps(a, Some("low"), a, Some("low"), None), []);
        assert_eq!(steps(a, Some("low"), a, Some("low"), Some("low")), []);
        assert_eq!(steps(a, Some("low"), a, Some("low"), Some("high")), [Step::Effort("high")]);
        assert_eq!(steps(a, Some("max"), a, Some("low"), None), [Step::Switch]);
        assert_eq!(steps(a, None, b, None, None), [Step::Switch]);
        assert_eq!(steps(a, None, b, Some("high"), Some("high")), [Step::Switch]);
        assert_eq!(steps(a, None, b, None, Some("high")), [Step::Switch, Step::Effort("high")]);
        assert_eq!(steps(a, Some("max"), ("q", "a"), None, Some("max")), [Step::Switch, Step::Effort("max")]);
    }
}
