use crate::agent::Report;
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_BYTES: usize = 32 * 1024;
const EXCHANGE: usize = 3968;

/// A user message that starts a task: not one of low's own, and not a line the user typed during a turn to steer it.
pub fn task(entry: &Value) -> bool {
    entry["role"] == "user" && entry["meta"]["host"].is_null() && entry["meta"]["steer"].is_null()
}

pub fn newest_task(entries: &[Value]) -> Option<usize> {
    (1..entries.len()).rev().find(|&i| task(&entries[i]))
}

/// What the user typed to steer task `u` before entry `keep`, in full, to follow the task wherever a summary carries it.
pub fn steers(entries: &[Value], u: usize, keep: usize) -> String {
    (u + 1..keep.min(entries.len()))
        .filter(|&i| entries[i]["meta"]["steer"] == true)
        .filter_map(|i| wire(&entries[i])["content"].as_str().map(|text| format!("\n\n{text}")))
        .collect()
}

pub fn baked(u: usize) -> String {
    format!("[Latest task when this summary was written, entry #{u}]")
}

/// An entry as the model sees it: no meta, and the reasoning whole, as DeepSeek asks for a request with tools. It
/// follows from the entry alone, so every request and every resume sends the same bytes for it, and they are the bytes
/// the provider streamed, so the prompt prefix stays cached through the reply. Earlier tasks keep their reasoning too,
/// until a compaction summarizes them: dropping it at a new task broke the cache from the first earlier reply on
/// (DeepSeek on OpenCode Go reused 34% of the prompt instead of 99%). A task ends with the date it was given on, kept
/// in its meta, so its text stays the task's everywhere else; a line the user typed during a turn starts with STEERED,
/// which tells the model it steers the task rather than starting a new one.
pub fn wire(entry: &Value) -> Value {
    let mut message = entry.clone();
    let date = entry["meta"]["date"].as_str().filter(|_| task(entry));
    if let Some(fields) = message.as_object_mut() {
        fields.remove("meta");
    }
    let text = entry["content"].as_str().unwrap_or_default();
    if let Some(date) = date {
        message["content"] = format!("{text}\n\n[Today is {date}]").into();
    } else if entry["meta"]["steer"] == true {
        message["content"] = format!("{STEERED}\n{text}").into();
    }
    message
}

/// What a steer starts with wherever the model reads it: as it is sent, in a summary, and in a second look's brief.
const STEERED: &str = "[Steered by the user]";

/// The entry the kept tail starts at: the earliest reply from which the rest fits in `keep_tokens`, and without `whole`
/// at least the last reply. Without `whole` it is `prev`, nothing to compact, when all of it fits: compaction keeps that
/// much as it is. `whole` follows an overflow, which shows the estimate ran short, so it cuts anyway. With no reply after
/// `prev` to cut at, the tail is a reply's own result, not yet answered: without `whole` that is nothing to compact, and
/// the model's next reply gives the next compaction a place to cut, past the result it has read.
pub fn cut(entries: &[Value], prev: usize, keep_tokens: usize, whole: bool) -> usize {
    let (mut tail, mut keep) = (0, entries.len());
    for i in (prev + 1..entries.len()).rev() {
        let Some(role) = entries[i]["role"].as_str() else { continue };
        tail += wire(&entries[i]).to_string().len() / 4;
        if tail > keep_tokens && (whole || keep < entries.len()) {
            break;
        }
        if role == "assistant" {
            keep = i;
        }
    }
    if whole || (tail > keep_tokens && keep < entries.len()) { keep } else { prev }
}

pub fn summary(entries: &[Value], keep: usize) -> String {
    let role = |i: usize| entries[i]["role"].as_str().unwrap_or_default();
    let text = |i: usize| entries[i]["content"].as_str().unwrap_or_default();
    let said = |i: usize| !text(i).trim().is_empty();
    let range = 1..keep;
    let users: Vec<usize> = range.clone().filter(|&i| task(&entries[i])).collect();
    let recent = &users[users.len().saturating_sub(3)..];
    let current = newest_task(entries).filter(|&u| u < keep);

    let (mut dialogue, mut used) = (vec![], 0);
    for (n, &u) in recent.iter().enumerate() {
        let budget = if n + 1 == recent.len() { (3 * EXCHANGE).saturating_sub(used) } else { EXCHANGE };
        let a = (1..u).rev().take_while(|&i| role(i) != "user").find(|&i| role(i) == "assistant" && said(i));
        let reply = a.map(|a| quote(text(a)));
        let room = reply.as_ref().map_or(EXCHANGE, |r| (EXCHANGE / 2).max(EXCHANGE.saturating_sub(r.len())));
        let user = if Some(u) == current {
            " [the latest task when this summary was written, in full below]".to_string()
        } else {
            format!("\n{}", excerpt(&quote(text(u)), room))
        };
        used += user.len();
        if let (Some(a), Some(reply)) = (a, reply) {
            let reply = excerpt(&reply, budget.saturating_sub(user.len()));
            used += reply.len();
            dialogue.push(format!("#{a} assistant (historical, not a verified outcome):\n{reply}"));
        }
        dialogue.push(format!("#{u} user:{user}"));
    }
    let after = users.last().map_or(1, |&u| u + 1);
    if let Some(a) = (after..keep).rev().find(|&i| role(i) == "assistant" && said(i)) {
        dialogue.push(format!(
            "#{a} assistant, latest reply (historical, not a verified outcome):\n{}",
            excerpt(&quote(text(a)), 4000)
        ));
    }
    let requests: Vec<String> = users[..users.len() - recent.len()]
        .iter()
        .map(|&i| format!("#{i}\n{}", excerpt(&quote(text(i)), 600)))
        .collect();
    let reported = entries.iter().any(|e| e["report"].is_object());
    let outcomes: Vec<String> = users
        .iter()
        .filter(|_| reported)
        .filter_map(|&u| {
            let next = (u + 1..entries.len()).find(|&i| task(&entries[i]));
            let end = next.unwrap_or(entries.len());
            let report = entries[u..end].iter().rev().find_map(Report::of);
            if report.is_none() && next.is_none() {
                return None;
            }
            let verdict = report.map_or("no verdict".to_string(), |r| match r.changes.len() {
                0 => r.verdict,
                changed => format!("{}, {changed} files changed", r.verdict),
            });
            Some(format!("#{u} {} → {verdict}", line(text(u), 80)))
        })
        .collect();

    let mut criteria = vec![];
    let mut files: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut failures: Vec<(String, String)> = vec![];
    let mut status = vec![];
    let mut failing_check = None;
    for i in range.clone() {
        let meta = &entries[i]["meta"];
        if task(&entries[i]) {
            criteria.clear();
        }
        if let Some(c) = meta["accepted"].as_str() {
            criteria = vec![format!("#{i}\n{}", excerpt(&quote(c), 1800))];
        }
        for act in meta["actions"].as_array().into_iter().flatten() {
            let (tool, path, ok) = (act["tool"].as_str().unwrap_or_default(), act["path"].as_str(), act["ok"] == true);
            let action = path.map_or_else(|| tool.to_string(), |p| format!("{tool} {p}"));
            match path {
                Some(p) if ok => {
                    touch(&mut files, p, if tool != "write" || act["replaced"] == true { 1 } else { 2 }, i);
                    status.retain(|s: &String| !s.starts_with("last change"));
                    status.push(format!("last change: #{i} {action}"));
                }
                None => {
                    status.retain(|s: &String| !s.starts_with("last check"));
                    status.push(format!("last check: #{i} {}", if ok { "passed" } else { "FAILED" }));
                    failing_check = (!ok).then_some(i);
                }
                _ => {}
            }
            failures.retain(|(a, _)| *a != action);
            if !ok {
                let problem = act["error"].as_str().map_or_else(|| format!("output at #{i}"), |e| line(e, 200));
                failures.push((action.clone(), format!("#{i} {action} failed: {problem}")));
            }
        }
    }
    if let Some(i) = range.clone().rev().find(|&i| role(i) == "assistant" && said(i)) {
        status.push(format!("last assistant note: #{i} {}", line(text(i), 160)));
    }
    let mut ranked: Vec<_> = files.iter().collect();
    ranked.sort_by_key(|(_, (rank, _))| std::cmp::Reverse(*rank));
    let files: Vec<String> =
        ranked.iter().map(|(path, (rank, i))| format!("{} {path} #{i}", ["", "modified", "created"][*rank])).collect();
    let check_output: Vec<String> =
        failing_check.map(|i| format!("#{i}\n{}", quote(tail(text(i), 1536)))).into_iter().collect();
    // The newest task's plan as plan() and done() left it, as Session::state() reads it: its steps are otherwise only in
    // the code of the run that called plan(), which a summary can replace.
    let mut plans = newest_task(entries).map_or(&[][..], |u| &entries[u..]).iter().rev().map(|e| &e["meta"]["plan"]);
    let live = plans.find(|p| p.is_object()).unwrap_or(&Value::Null);
    let done: Vec<u64> = live["done"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    let steps = live["steps"].as_array().map_or(&[][..], Vec::as_slice);
    let next = (1..=steps.len() as u64).find(|n| !done.contains(n));
    let plan: Vec<String> = (1..)
        .zip(steps)
        .map(|(n, step)| {
            let mark = if done.contains(&n) {
                "✓"
            } else if next == Some(n) {
                "▸"
            } else {
                "·"
            };
            format!("{mark} {n}. {}", line(step.as_str().unwrap_or_default(), 160))
        })
        .collect();

    // (entry, what happened)
    let mut events = vec![];
    for i in range {
        match role(i) {
            "user" => {
                let who = if entries[i]["meta"]["host"].is_null() { "user" } else { "host" };
                events.push((i, format!("{who}: {}", line(text(i), 100))));
            }
            "assistant" => {
                if said(i) {
                    events.push((i, format!("assistant: {}", line(text(i), 100))));
                }
                for call in entries[i]["tool_calls"].as_array().into_iter().flatten() {
                    let name = call["function"]["name"].as_str().unwrap_or_default();
                    let args: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or_default())
                        .unwrap_or_default();
                    let code = args["code"].as_str().unwrap_or_default();
                    let what = line(crate::ui::headline(code).unwrap_or_default(), 80);
                    let result = (i + 1..keep)
                        .take_while(|&r| matches!(role(r), "tool" | ""))
                        .find(|&r| entries[r]["tool_call_id"] == call["id"]);
                    let outcome = result.map_or_else(
                        || "no result".into(),
                        |r| {
                            let out = text(r);
                            let shown = if entries[r]["meta"]["error"] == true {
                                let last = |l: &&str| !l.is_empty() && !l.starts_with("hint:");
                                out.lines().map(str::trim).rev().find(last).unwrap_or_default()
                            } else {
                                out
                            };
                            line(shown, 80)
                        },
                    );
                    events.push((i, format!("{name} {what} → {outcome}")));
                }
            }
            _ => {}
        }
    }
    // Events that read the same back to back, as when a model stuck in a loop makes the same call and gets the same
    // result, show once with their count, which leaves the window to the events around them.
    let mut events: Vec<String> = events
        .chunk_by(|a, b| a.1 == b.1)
        .map(|run| {
            let ((first, event), last) = (&run[0], run[run.len() - 1].0);
            let at = if last > *first { format!("#{first}-#{last}") } else { format!("#{first}") };
            match run.len() {
                1 => format!("{at} {event}"),
                n => format!("{at} {event} ({n} times)"),
            }
        })
        .collect();
    let events = events.split_off(events.len().saturating_sub(40));

    let footer = format!(
        "\n[Entries #1-#{} are summarized here. recall(n) prints entry #n as it was: a message, or a run's code, result \
         and reasoning, paged like show(). Read a file again instead, since it may have changed. Continue the task.]",
        keep - 1
    );
    let mut out = String::from("[Summary of earlier work, rebuilt from the session log. Newer messages follow it.]\n");
    for (title, lines, cap) in [
        ("Recent user directions", dialogue, 16_384),
        ("Accepted criteria", criteria, 2_048),
        ("Plan (✓ done, ▸ next)", plan, 1_536),
        ("Unresolved failures", failures.into_iter().map(|(_, l)| l).collect(), 3_072),
        ("Failing check output", check_output, 3_200),
        ("Status", status, 1_536),
        ("Files", files, 4_096),
        ("Earlier requests", requests, 6_144),
        ("Turn outcomes (host verdicts)", outcomes, 1_536),
        ("Latest events", events, 4_096),
    ] {
        let left = MAX_BYTES.saturating_sub(out.len() + footer.len() + 1);
        let section = block(&format!("\n## {title}"), &lines, cap.min(left));
        if !lines.is_empty() && !section.is_empty() {
            out += &section;
            out += "\n";
        }
    }
    out += &footer;
    if let Some(u) = current {
        out += &format!("\n\n{}\n{}{}", baked(u), text(u), steers(entries, u, keep));
    }
    out
}

/// Entry `n` whole, as recall(n) prints it, or why it can't: only what a summary replaced, entries before `keep`, since
/// the context holds the rest. A message as the model got it; a reply with each run's code and result, and its
/// reasoning last, whether `n` is the reply or a result, since a summary cites a run by either. No LLM call: it is the
/// log as written, already redacted.
pub fn recall(entries: &[Value], keep: usize, n: usize) -> Result<String, String> {
    let last = entries.len().saturating_sub(1);
    if n == 0 || n > last {
        return Err(format!("no such entry: this session's run from #1 to #{last}"));
    }
    if n >= keep {
        return Err(match keep {
            ..=1 => "nothing is summarized yet: every entry is still in your context".to_string(),
            _ => format!("still in your context; recall() opens #1-#{}, which a summary replaced", keep - 1),
        });
    }
    let role = |i: usize| entries[i]["role"].as_str().unwrap_or_default();
    let calls = |i: usize| entries[i]["tool_calls"].as_array().map_or(&[][..], Vec::as_slice);
    let reply = match role(n) {
        "user" => {
            let who = if entries[n]["meta"]["host"].is_null() { "the user" } else { "low" };
            let text = wire(&entries[n]);
            return Ok(format!("[#{n}: a message from {who}]\n{}", text["content"].as_str().unwrap_or_default()));
        }
        "assistant" => n,
        "tool" => {
            let id = &entries[n]["tool_call_id"];
            let made = (1..n).rev().find(|&i| role(i) == "assistant" && calls(i).iter().any(|c| c["id"] == *id));
            let Some(a) = made else {
                return Ok(format!("[#{n}: a run's result]\n{}", entries[n]["content"].as_str().unwrap_or_default()));
            };
            a
        }
        _ => return Err("low's own record, not a message".into()),
    };
    let entry = &entries[reply];
    let mut out = vec![];
    if let Some(text) = entry["content"].as_str().filter(|t| !t.trim().is_empty()) {
        out.push(format!("[#{reply}: your reply]\n{text}"));
    }
    for call in calls(reply) {
        let args = call["function"]["arguments"].as_str().unwrap_or_default();
        let parsed: Value = serde_json::from_str(args).unwrap_or_default();
        out.push(format!("[#{reply}: the run's code]\n{}", parsed["code"].as_str().unwrap_or(args)));
        let result = (reply + 1..entries.len())
            .take_while(|&r| matches!(role(r), "tool" | ""))
            .find(|&r| entries[r]["tool_call_id"] == call["id"]);
        out.push(match result {
            Some(r) => format!("[#{r}: its result]\n{}", entries[r]["content"].as_str().unwrap_or_default()),
            None => "[no result: the run was cut off]".into(),
        });
    }
    let reasoning = ["reasoning_content", "reasoning"].iter().find_map(|f| entry[*f].as_str());
    if let Some(text) = reasoning.filter(|t| !t.trim().is_empty()) {
        out.push(format!("[#{reply}: your reasoning]\n{text}"));
    }
    if out.is_empty() {
        out.push(format!("[#{reply}: an empty reply]"));
    }
    Ok(out.join("\n"))
}

fn touch<'a>(files: &mut BTreeMap<&'a str, (usize, usize)>, path: &'a str, rank: usize, i: usize) {
    let seen = files.entry(path).or_insert((rank, i));
    *seen = (seen.0.max(rank), i);
}

fn block(title: &str, lines: &[String], cap: usize) -> String {
    let n = lines.len();
    // The joined length of every candidate from running sums, so that only the one that fits is joined.
    let mut sums = vec![0];
    sums.extend(lines.iter().scan(0, |sum, line| {
        *sum += line.len() + 1;
        Some(*sum)
    }));
    let omitted = |keep: usize| if keep < n { format!("… {} omitted", n - keep).len() + 1 } else { 0 };
    let size = |keep: usize| title.len() + sums[keep.div_ceil(2)] + omitted(keep) + sums[n] - sums[n - keep / 2];
    let Some(keep) = (0..=n).rev().find(|&keep| size(keep) <= cap) else { return String::new() };
    let mut kept = vec![title.to_string()];
    kept.extend_from_slice(&lines[..keep.div_ceil(2)]);
    if keep < n {
        kept.push(format!("… {} omitted", n - keep));
    }
    kept.extend_from_slice(&lines[n - keep / 2..]);
    kept.join("\n")
}

pub fn quote(s: &str) -> String {
    s.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n")
}

fn excerpt(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = clip(s, max.saturating_sub(40));
    format!("{head}\n[… {} bytes omitted]", s.len() - head.len())
}

fn line(s: &str, max: usize) -> String {
    let first = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    if first.len() <= max { first.to_string() } else { format!("{}…", clip(first, max)) }
}

fn clip(s: &str, max: usize) -> &str {
    &s[..s.floor_char_boundary(max)]
}

pub fn tail(s: &str, max: usize) -> &str {
    &s[s.ceil_char_boundary(s.len().saturating_sub(max))..]
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    /// block() as it was before its running sums: the most lines that fit, found by joining every candidate.
    fn joined(title: &str, lines: &[String], cap: usize) -> String {
        let n = lines.len();
        for keep in (0..=n).rev() {
            let mut kept = vec![title.to_string()];
            kept.extend_from_slice(&lines[..keep.div_ceil(2)]);
            if keep < n {
                kept.push(format!("… {} omitted", n - keep));
            }
            kept.extend_from_slice(&lines[n - keep / 2..]);
            let block = kept.join("\n");
            if block.len() <= cap {
                return block;
            }
        }
        String::new()
    }

    #[test]
    fn block_matches_joining_every_candidate() {
        let lines: Vec<String> = (0..40).map(|i| "é".repeat(i % 7) + &format!("line {i}")).collect();
        for n in [0, 1, 2, 3, 10, 40] {
            for cap in (0..700).step_by(7) {
                assert_eq!(super::block("\n## T", &lines[..n], cap), joined("\n## T", &lines[..n], cap), "{n} {cap}");
            }
        }
    }

    /// A session of `tasks` tasks, each a user message, a reply with a run, its result and a final reply.
    fn session(tasks: usize) -> Vec<Value> {
        let mut entries = vec![json!({"provider": "p", "model": "m", "system": "s"})];
        for t in 0..tasks {
            entries.push(json!({"role": "user", "content": format!("task {t}: fix bug {t}"), "meta": {}}));
            let call = json!([{"id": format!("c{t}"), "type": "function",
                               "function": {"name": "run", "arguments": "{\"code\": \"check()\"}"}}]);
            entries.push(json!({"role": "assistant", "content": "", "tool_calls": call, "meta": {}}));
            let actions = json!([{"tool": "edit", "path": format!("src/f{}.rs", t % 5), "ok": true}]);
            let meta = json!({"actions": actions, "accepted": format!("bug {t} fixed")});
            entries.push(json!({"role": "tool", "tool_call_id": format!("c{t}"), "content": "ok", "meta": meta}));
            entries.push(json!({"role": "assistant", "content": format!("fixed bug {t}"), "meta": {}}));
        }
        entries
    }

    #[test]
    fn newest_task_skips_host_messages() {
        let mut entries = session(3);
        assert_eq!(super::newest_task(&entries), Some(9));
        entries.push(json!({"role": "user", "content": "nudge", "meta": {"host": true}}));
        assert_eq!(super::newest_task(&entries), Some(9));
        assert_eq!(super::newest_task(&entries[..1]), None);
    }

    #[test]
    fn steers_stay_with_their_task() {
        let mut entries = session(3);
        entries.push(json!({"role": "user", "content": "use tabs, not spaces", "meta": {"steer": true}}));
        entries.push(json!({"role": "assistant", "content": "ok", "meta": {}}));
        assert_eq!(super::newest_task(&entries), Some(9));
        let summary = super::summary(&entries, entries.len() - 1);
        let tail = format!("{}\ntask 2: fix bug 2\n\n[Steered by the user]\nuse tabs, not spaces", super::baked(9));
        assert!(summary.ends_with(&tail), "{summary}");
        // A steer past the kept tail's start is sent as it is, not in the summary.
        assert!(!super::summary(&entries, 13).contains("[Steered by the user"));
    }

    #[test]
    fn cut_keeps_whole_replies() {
        let entries = session(20);
        let keep = super::cut(&entries, 1, 100, false);
        assert!(keep > 1 && keep < entries.len() && entries[keep]["role"] == "assistant", "{keep}");
        // Everything fits: nothing to compact, unless an overflow asks for a cut anyway.
        assert_eq!(super::cut(&entries, 1, 1 << 30, false), 1);
        assert!(super::cut(&entries, 1, 0, true) > 1);
    }

    /// A compaction whose kept tail is one reply and its large result leaves the next cut nothing to do, rather than
    /// keeping nothing; an overflow still cuts it all.
    #[test]
    fn cut_keeps_the_last_reply() {
        let big = "line of output\n".repeat(2000);
        let mut entries = vec![
            json!({"system": "s"}),
            json!({"role": "user", "content": "earlier task", "meta": {}}),
            json!({"role": "assistant", "content": "earlier answer", "meta": {}}),
            json!({"role": "user", "content": "task", "meta": {}}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "type": "function",
                "function": {"name": "run", "arguments": "{\"code\": \"print(big())\"}"}}], "meta": {}}),
            json!({"role": "tool", "tool_call_id": "c1", "content": big, "meta": {"tool": "run"}}),
        ];
        let first = super::cut(&entries, 1, 200, false);
        entries.push(json!({"compact": {"keep": first, "summary": super::summary(&entries, first)}}));
        assert_eq!((first, super::cut(&entries, first, 200, false)), (4, 4));
        assert_eq!(super::cut(&entries, first, 200, true), entries.len());
    }

    #[test]
    fn a_task_carries_its_date() {
        let task = json!({"role": "user", "content": "fix it", "meta": {"date": "2026-10-19", "snapshot": "t"}});
        assert_eq!(super::wire(&task), json!({"role": "user", "content": "fix it\n\n[Today is 2026-10-19]"}));
        // A session from before dates and low's own messages are sent as they were; a steer says it is one.
        let dated = |flag: &str| json!({"date": "2026-10-19", flag: true});
        for meta in [json!({}), dated("host")] {
            let entry = json!({"role": "user", "content": "fix it", "meta": meta});
            assert_eq!(super::wire(&entry), json!({"role": "user", "content": "fix it"}), "{meta}");
        }
        let steer = json!({"role": "user", "content": "use tabs", "meta": dated("steer")});
        assert_eq!(super::wire(&steer)["content"], "[Steered by the user]\nuse tabs");
    }

    #[test]
    fn recall_opens_what_a_summary_replaced() {
        let mut entries = session(3);
        entries[2]["reasoning_content"] = json!("the bug is in f0");
        entries[5]["meta"] = json!({"date": "2026-10-19"});
        entries.push(json!({"role": "user", "content": "go on", "meta": {"host": true}}));
        entries.push(json!({"event": "worker", "text": "python worker ready"}));
        let recall = |keep: usize, n: usize| super::recall(&entries, keep, n);
        // A run whole, its reasoning last, whether the summary cites the reply that made it or its result.
        let run = "[#2: the run's code]\ncheck()\n[#3: its result]\nok\n[#2: your reasoning]\nthe bug is in f0";
        assert_eq!((recall(9, 2).as_deref(), recall(9, 3).as_deref()), (Ok(run), Ok(run)));
        assert_eq!(recall(9, 4).as_deref(), Ok("[#4: your reply]\nfixed bug 0"));
        // A message as the model got it.
        let task = "[#5: a message from the user]\ntask 1: fix bug 1\n\n[Today is 2026-10-19]";
        assert_eq!(recall(9, 5).as_deref(), Ok(task));
        assert_eq!(recall(15, 13).as_deref(), Ok("[#13: a message from low]\ngo on"));
        // What the context still holds, low's own records and entries that don't exist are refused.
        assert_eq!(recall(15, 14), Err("low's own record, not a message".to_string()));
        for n in [0, 9, 12, 15, 99] {
            assert!(recall(9, n).is_err(), "{n}");
        }
        assert!(recall(1, 2).is_err_and(|why| why.starts_with("nothing is summarized yet")));
    }

    /// Three tasks that fill their exchange budgets: the last exchange's budget runs out without a panic.
    #[test]
    fn summary_budget_saturates() {
        let task = |n: usize| "t".repeat(n);
        let entries = vec![
            json!({"system": "s"}),
            json!({"role": "user", "content": task(3966), "meta": {}}),
            json!({"role": "assistant", "content": "ok", "meta": {}}),
            json!({"role": "user", "content": task(3962), "meta": {}}),
            json!({"role": "assistant", "content": "ok", "meta": {}}),
            json!({"role": "user", "content": task(3962), "meta": {}}),
            json!({"role": "assistant", "content": "ok", "meta": {}}),
            json!({"role": "user", "content": "the next task", "meta": {}}),
        ];
        let summary = super::summary(&entries, 7);
        assert!(summary.contains("#5 user:") && summary.len() <= super::MAX_BYTES + 200, "{}", summary.len());
    }

    #[test]
    fn summary_is_bounded_and_deterministic() {
        for tasks in [1, 3, 50, 400] {
            let entries = session(tasks);
            let keep = entries.len() - 1;
            let summary = super::summary(&entries, keep);
            assert_eq!(summary, super::summary(&entries, keep));
            assert!(summary.len() <= super::MAX_BYTES + 200, "{tasks}: {}", summary.len());
            assert!(summary.contains(&format!("[Entries #1-#{} are summarized here.", keep - 1)), "{summary}");
            let latest = super::newest_task(&entries).unwrap();
            assert!(summary.contains(&super::baked(latest)) && summary.contains(&format!("task {}", tasks - 1)));
            assert!(summary.contains("## Files") && summary.contains("src/f0.rs"), "{summary}");
        }
    }
}
