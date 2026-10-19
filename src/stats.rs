use serde_json::{Value, json};
use std::collections::BTreeMap;

/// One run's totals, for the end record of its session log: requests, tokens and speed, what the runs and checks
/// did, and every event by kind (notes are the warnings shown to the user).
pub fn totals(entries: &[Value]) -> Value {
    let (mut n, mut events, mut models) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let (mut verdicts, mut exceptions, mut cost) = (BTreeMap::new(), BTreeMap::new(), None::<f64>);
    let count = |map: &mut BTreeMap<String, u64>, key: &str, by: u64| *map.entry(key.to_string()).or_default() += by;
    for e in entries {
        let (meta, usage, timing) = (&e["meta"], &e["meta"]["usage"], &e["meta"]["timing"]);
        if let Some(kind) = e["event"].as_str() {
            count(&mut events, kind, 1);
        }
        if let Some(report) = crate::agent::Report::of(e) {
            count(&mut verdicts, &report.verdict, 1);
        }
        count(&mut n, "turns", u64::from(crate::compact::task(e)));
        count(&mut n, "compactions", u64::from(e["compact"].is_object()));
        if usage.is_object() {
            let out = usage["completion_tokens"].as_u64();
            count(&mut n, "replies", 1);
            count(&mut n, "prompt", usage["prompt_tokens"].as_u64().unwrap_or(0));
            count(&mut n, "cached", crate::ui::cached(usage).unwrap_or(0));
            count(&mut n, "out", out.unwrap_or(0));
            count(&mut n, "reasoning", usage["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0));
            cost = usage["cost"].as_f64().map(|c| cost.unwrap_or(0.0) + c).or(cost);
            if let Some(model) = meta["request"]["model"].as_str() {
                count(&mut models, model, 1);
            }
            if let Some(ttft) = timing["ttft_ms"].as_u64() {
                count(&mut n, "timed", 1);
                count(&mut n, "ttft_ms", ttft);
                if let (Some(out), Some(stream)) = (out, timing["stream_ms"].as_u64()) {
                    count(&mut n, "decoded", out);
                    count(&mut n, "decode_ms", stream.saturating_sub(ttft));
                }
            }
        }
        if meta["tool"] == "run" {
            count(&mut n, "runs", 1);
            count(&mut n, "run_errors", u64::from(meta["error"] == true));
            count(&mut n, "rejected", u64::from(meta["rejected"] == true));
            count(&mut n, "redacted", meta["redacted"].as_u64().unwrap_or(0));
            if let Some(name) = meta["exception"].as_str() {
                count(&mut exceptions, name, 1);
            }
            for act in meta["actions"].as_array().into_iter().flatten().filter(|a| a["path"].is_string()) {
                count(&mut n, if act["ok"] == true { "edits_ok" } else { "edits_failed" }, 1);
            }
            for cmd in meta["cmds"].as_array().into_iter().flatten() {
                count(&mut n, if cmd[1] == 0 { "shell_ok" } else { "shell_failed" }, 1);
            }
        }
        let check = &e["check"];
        if check.is_object() {
            let outcome = match (check["reused"] == true, check["ok"] == true) {
                (true, _) => "checks_reused",
                (false, true) => "checks_passed",
                (false, false) => "checks_failed",
            };
            count(&mut n, outcome, 1);
        }
    }
    let get = |key: &str| n.get(key).copied().unwrap_or(0);
    let lost: u64 = events.get("interrupted").copied().unwrap_or(0);
    let tps =
        (get("decode_ms") > 0).then(|| (get("decoded") as f64 * 10_000.0 / get("decode_ms") as f64).round() / 10.0);
    json!({
        "turns": get("turns"), "verdicts": verdicts, "requests": get("replies") + lost, "models": models,
        "tokens": {"prompt": get("prompt"), "cached": get("cached"), "out": get("out"), "reasoning": get("reasoning")},
        "cost": cost, "ttft_ms": get("ttft_ms").checked_div(get("timed")), "tps": tps,
        "runs": get("runs"), "run_errors": get("run_errors"), "rejected": get("rejected"), "exceptions": exceptions,
        "edits": {"ok": get("edits_ok"), "failed": get("edits_failed")},
        "shell": {"ok": get("shell_ok"), "failed": get("shell_failed")},
        "checks": {"passed": get("checks_passed"), "failed": get("checks_failed"), "reused": get("checks_reused")},
        "compactions": get("compactions"), "redacted": get("redacted"), "events": events,
    })
}
