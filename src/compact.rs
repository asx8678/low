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
    let steer = |i: &usize| entries[*i]["meta"]["steer"] == true;
    (u + 1..keep.min(entries.len()))
        .filter(steer)
        .map(|i| {
            format!("\n\n[Steered by the user, entry #{i}]\n{}", entries[i]["content"].as_str().unwrap_or_default())
        })
        .collect()
}

pub fn baked(u: usize) -> String {
    format!("[Latest task when this summary was written, entry #{u}]")
}

/// An entry as the model sees it: no meta, and the reasoning whole, as DeepSeek asks for a request with tools. It
/// follows from the entry alone, so every request and every resume sends the same bytes for it, and they are the bytes
/// the provider streamed, so the prompt prefix stays cached through the reply. Earlier tasks keep their reasoning too,
/// until a compaction summarizes them: dropping it at a new task broke the cache from the first earlier reply on
/// (DeepSeek on OpenCode Go reused 34% of the prompt instead of 99%).
pub fn wire(entry: &Value) -> Value {
    let mut message = entry.clone();
    if let Some(fields) = message.as_object_mut() {
        fields.remove("meta");
    }
    message
}

/// The entry the kept tail starts at: the earliest reply from which the rest fits in `keep_tokens`, and without `whole`
/// at least the last reply. Without `whole` it is `prev`, nothing to compact, when all of it fits: compaction keeps that
/// much as it is. `whole` follows an overflow, which shows the estimate ran short, so it cuts anyway.
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
    if tail > keep_tokens || whole { keep } else { prev }
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
        let budget = if n + 1 == recent.len() { 3 * EXCHANGE - used } else { EXCHANGE };
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
            let reply = excerpt(&reply, budget - user.len());
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
                    touch(&mut files, p, if tool == "edit" || act["replaced"] == true { 1 } else { 2 }, i);
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

    let footer = format!("\n[Entries #1-#{} are summarized here. Continue the task.]", keep - 1);
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
        let tail =
            format!("{}\ntask 2: fix bug 2\n\n[Steered by the user, entry #13]\nuse tabs, not spaces", super::baked(9));
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
