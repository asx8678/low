use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

pub const TEMPERATURE: f64 = 0.0;
/// What low calls itself on the wire.
const USER_AGENT: &str = concat!("low/", env!("CARGO_PKG_VERSION"));
const LONG: Duration = Duration::from_secs(150);
const FIRST_BYTE: Duration = Duration::from_secs(90);
const IDLE: Duration = Duration::from_mins(5);
const TOTAL: Duration = Duration::from_hours(1);
const LINE_MAX: usize = 16 << 20;
const BODY_MAX: usize = 2000;
const FIELDS: [&str; 2] = ["reasoning_content", "reasoning"];
/// The line a warm request adds after the conversation; its one-token reply is dropped.
const WARM: &str = "Reply with one word: ok";
const OVERFLOW: &[&str] = &[
    "context_length",
    "context length",
    "context window",
    "context size",
    "maximum context",
    "prompt is too long",
    "too many tokens",
    "exceeds the maximum number of tokens",
];
const WINDOW: &[&str] = &[
    "context length is",
    "context length of",
    "context window is",
    "context window of",
    "\"n_ctx\":",
    "tokens allowed (",
];
const QUOTA: &str = "insufficient_quota";
/// Error words for a provider that is out of credit or quota for this key, as opposed to briefly rate-limited.
const EXHAUSTED: &[&str] =
    &[QUOTA, "key_quota_exceeded", "insufficient_balance", "insufficient balance", "spend limit"];
/// OpenCode Go's gateway adds a prompt_cache_key field to requests, and some upstreams refuse it with a 400. Another
/// try may reach one that takes it; low tries once more.
const CACHE_KEY: &str = "prompt_cache_key";
const REJECTED: &[&str] = &["invalid", "authentication", "permission", "not_found", "insufficient_quota"];
/// Stream chunk fields low reads itself or that carry nothing; the rest (id, model, provider, …) go in the log.
const CHUNK: &[&str] = &["choices", "object", "created", "usage", "error", "obfuscation"];
const HEADERS: &[&str] = &["request-id", "cf-ray", "server", "via", "retry-after", "server-timing"];

/// The provider refused for want of credit or quota on this key: retrying won't help, another provider might.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Exhausted(String);

/// The provider refused the request as too long; `window` is the context size its error states, if any.
#[derive(Debug, thiserror::Error)]
#[error("context window exceeded: {text}")]
pub struct ContextOverflow {
    text: String,
    pub window: Option<u64>,
}

impl ContextOverflow {
    fn new(text: String) -> Self {
        Self { window: window(&text), text }
    }
}

#[derive(Debug, Default)]
pub struct Lost {
    pub thinking: String,
    pub text: usize,
    pub stalled: bool,
    pub ms: u64,
    pub response: Value,
}

impl Lost {
    fn new(thinking: &str, began: Option<Instant>) -> Self {
        Self { thinking: thinking.into(), ms: began.map_or(0, crate::ui::ms), ..Default::default() }
    }

    pub fn extra(&self) -> Value {
        json!({
            "chars": self.thinking.chars().count(), "text_chars": self.text, "stalled": self.stalled,
            "think_ms": self.ms, "response": self.response,
        })
    }
}

pub fn lost(e: &anyhow::Error) -> Option<&Lost> {
    e.downcast_ref::<Interrupted>().map(|i| &i.1)
}

#[derive(Debug, thiserror::Error)]
#[error("stream interrupted: {0}")]
pub struct Interrupted(String, Lost);

pub fn params(header: &Value) -> Value {
    match &header["params"] {
        Value::Object(params) => params.clone().into(),
        _ => json!({"temperature": header["temperature"].as_f64().unwrap_or(TEMPERATURE)}),
    }
}

/// OpenCode's gateway refuses requests without `x-opencode-session`, a stable id per
/// conversation it routes by. That is its protocol, not low's identity — the User-Agent above
/// is where low names itself.
fn session_id(base_url: &str, header: &Value) -> Option<String> {
    let host = base_url.rsplit_once("://").map_or(base_url, |(_, rest)| rest);
    let host = host.split('/').next().unwrap_or("");
    if host != "opencode.ai" && !host.ends_with(".opencode.ai") {
        return None;
    }
    header["id"].as_str().filter(|id| !id.is_empty()).map(String::from)
}

fn overflow(text: &str) -> bool {
    let lower = text.to_lowercase();
    OVERFLOW.iter().any(|m| lower.contains(m))
}

fn window(error: &str) -> Option<u64> {
    let lower = error.to_lowercase();
    WINDOW.iter().find_map(|marker| {
        let rest = lower[lower.find(marker)? + marker.len()..].trim_start();
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == ',').collect();
        let run = digits.trim_end_matches(',');
        let n: u64 = run.chars().filter(char::is_ascii_digit).collect::<String>().parse().ok()?;
        let n = match rest[run.len()..].chars().next() {
            Some('k') => n * 1_000,
            Some('m') => n * 1_000_000,
            _ => n,
        };
        // Under 1024 tokens it is a misread, not a context window.
        (n >= 1024).then_some(n)
    })
}

/// One call to DeepSeek's tools API (search or fetch) at this provider: the response body, or why it failed.
pub async fn tool(base_url: &str, key: Option<&str>, name: &str, body: &Value) -> Result<String> {
    let base = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let http = reqwest::Client::builder().user_agent(USER_AGENT).timeout(Duration::from_secs(60)).build()?;
    let mut request = http.post(format!("{base}/v1/tools/{name}")).header("content-type", "application/json");
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let response = request.body(serde_json::to_vec(body)?).send().await.map_err(|e| anyhow!(chain(e)))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{status}: {}", head(response).await);
    }
    response.text().await.map_err(|e| anyhow!(chain(e)))
}

pub fn key_file() -> std::path::PathBuf {
    crate::providers::config_dir(&crate::worker::home()).join("api_key")
}

/// The URL without credentials, fit for the session log.
pub fn endpoint(url: &str) -> String {
    let mut parsed = reqwest::Url::parse(url).ok();
    if let Some(url) = parsed.as_mut() {
        let _ = url.set_username("");
        let _ = url.set_password(None);
    }
    crate::redact::redact(&parsed.map_or_else(|| url.to_string(), String::from)).0
}

/// The response headers worth keeping: request ids, rate limits, timing and routing; never cookies or credentials.
fn headers(map: &reqwest::header::HeaderMap) -> Value {
    let kept = map.iter().filter(|(name, _)| {
        let name = name.as_str();
        let wanted = ["x-", "openai-", "anthropic-"].iter().any(|p| name.starts_with(p)) || HEADERS.contains(&name);
        // Not "token" anywhere: x-ratelimit-remaining-tokens is the point; x-csrf-token is not.
        let secret = ["cookie", "auth", "key", "secret"].iter().any(|s| name.contains(s)) || name.ends_with("token");
        wanted && !secret
    });
    let value =
        |v: &reqwest::header::HeaderValue| clip(&crate::redact::redact(&String::from_utf8_lossy(v.as_bytes())).0);
    kept.map(|(name, v)| (name.as_str().to_string(), Value::from(value(v)))).collect()
}

pub struct Llm {
    http: reqwest::Client,
    base: String,
    key: Option<String>,
    model: String,
    params: Value,
    session: Option<String>,
    field: Cell<&'static str>,
    marked: Cell<bool>,
    /// The provider refused stream_options, so requests go without it and the usage is estimated.
    plain: Cell<bool>,
}

impl Llm {
    pub fn new(base_url: &str, key: Option<&str>, header: &Value) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .user_agent(USER_AGENT)
                .build()
                .expect("a plain rustls client builds"),
            base: base_url.trim_end_matches('/').into(),
            key: key
                .map(String::from)
                .or_else(|| std::env::var("LOW_API_KEY").ok())
                .or_else(|| std::fs::read_to_string(key_file()).ok())
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty()),
            model: header["model"].as_str().unwrap_or_default().into(),
            params: params(header),
            session: session_id(base_url, header),
            field: Cell::new(FIELDS[0]),
            marked: Default::default(),
            plain: Default::default(),
        }
    }

    /// Sends reasoning back under the field this endpoint used last in the session, so that after a resume, a model
    /// switch or an /effort the first request carries the same bytes as before (the prompt stays cached) and the field
    /// the provider expects.
    pub fn recall(&self, entries: &[Value]) {
        let url = endpoint(&format!("{}/chat/completions", self.base));
        let mut ours = entries.iter().rev().filter(|e| e["meta"]["request"]["url"] == url.as_str());
        let used = ours.find_map(|e| e["meta"]["reasoning_field"].as_str());
        if let Some(field) = used.and_then(|used| FIELDS.into_iter().find(|f| *f == used)) {
            self.field.set(field);
        }
    }

    /// What every request carries besides the user agent: OpenCode's session id and the API key.
    fn authorized(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(id) = &self.session {
            request = request.header("x-opencode-session", id.as_str());
        }
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        request
    }

    pub async fn models(&self) -> Result<Vec<String>> {
        let request = self.authorized(self.http.get(format!("{}/models", self.base)).timeout(Duration::from_secs(15)));
        let fetch = async { anyhow::Ok(request.send().await?.error_for_status()?.text().await?) };
        let text = tokio::select! {
            text = fetch => text?,
            _ = tokio::signal::ctrl_c() => bail!("interrupted"),
        };
        let list: Value = serde_json::from_str(&text)?;
        let ids = list["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str());
        Ok(ids.map(String::from).collect())
    }

    /// A request's body: the session's parameters, the model, the messages with reasoning under the field this provider
    /// uses, the tools, and a streamed reply that ends with its usage.
    fn body(&self, mut messages: Vec<Value>, tools: &Value) -> serde_json::Map<String, Value> {
        let field = self.field.get();
        for fields in messages.iter_mut().filter(|_| field != FIELDS[0]).filter_map(Value::as_object_mut) {
            if let Some(reasoning) = fields.remove(FIELDS[0]) {
                fields.insert(field.into(), reasoning);
            }
        }
        let mut body = self.params.as_object().cloned().unwrap_or_default();
        body.insert("model".into(), self.model.as_str().into());
        body.insert("messages".into(), Value::Array(messages));
        body.insert("tools".into(), tools.clone());
        body.insert("stream".into(), true.into());
        if !self.plain.get() {
            body.insert("stream_options".into(), json!({"include_usage": true}));
        }
        body
    }

    /// Sends the conversation so far plus a throwaway line, cut at one output token, so the provider's prompt cache
    /// keeps the prefix that the next task's first request shares. Returns the request's usage and time, as a reply's
    /// meta.
    pub async fn warm(&self, mut messages: Vec<Value>, tools: &Value) -> Result<Value> {
        messages.push(json!({"role": "user", "content": WARM}));
        let mut body = self.body(messages, tools);
        body.insert("max_tokens".into(), 1.into());
        let url = format!("{}/chat/completions", self.base);
        let post = self.http.post(&url).header("content-type", "application/json").timeout(Duration::from_secs(120));
        let start = Instant::now();
        let mut response =
            self.authorized(post.body(serde_json::to_vec(&body)?)).send().await.map_err(|e| anyhow!(chain(e)))?;
        if !response.status().is_success() {
            bail!("{}: {}", response.status(), head(response).await);
        }
        let (mut lines, mut usage) = (Lines::default(), Value::Null);
        while let Some(chunk) = response.chunk().await.map_err(|e| anyhow!(chain(e)))? {
            lines.push(&chunk);
            while let Some(line) = lines.next() {
                let event =
                    line.trim().strip_prefix("data:").and_then(|d| serde_json::from_str::<Value>(d.trim()).ok());
                if let Some(event) = event.filter(|e| e["usage"].is_object()) {
                    usage = event["usage"].clone();
                }
            }
        }
        let asked = json!({"url": endpoint(&url), "model": self.model});
        Ok(json!({"usage": usage, "timing": {"ms": crate::ui::ms(start)}, "request": asked}))
    }

    pub async fn complete(&self, messages: Vec<Value>, tools: &Value) -> Result<Value> {
        let count = messages.len();
        let mut body = self.body(messages, tools);
        let url = format!("{}/chat/completions", self.base);
        let start = Instant::now();
        for attempt in 1.. {
            let bytes = serde_json::to_vec(&body)?;
            let asked = json!({"url": endpoint(&url), "model": self.model, "messages": count, "bytes": bytes.len()});
            let request = self.authorized(self.http.post(&url).header("content-type", "application/json").body(bytes));
            let began = Instant::now();
            match self.stream(&request).await {
                // A gateway that refuses stream_options gets the request without it, for the rest of the session.
                Err(e) if !self.plain.get() && e.to_string().contains("stream_options") => {
                    self.plain.set(true);
                    body.remove("stream_options");
                    let text = "the provider refused stream_options; sending requests without it (usage is estimated)";
                    crate::ui::event("downgrade", text, json!({"error": e.to_string()}));
                }
                Err(e) if let Some(mut extra) = lost(&e).map(Lost::extra) => {
                    extra["attempt"] = attempt.into();
                    extra["ms"] = crate::ui::ms(began).into();
                    extra["request"] = asked.clone();
                    if attempt == 4 || began.elapsed() > LONG {
                        crate::ui::event("interrupted", &e.to_string(), extra);
                        return Err(e);
                    }
                    crate::ui::event("interrupted", &format!("{e}; retrying from the start"), extra);
                    tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
                }
                Ok(mut message) => {
                    message["meta"]["timing"]["ms"] = crate::ui::ms(start).into();
                    message["meta"]["timing"]["attempts"] = attempt.into();
                    message["meta"]["request"] = asked;
                    return Ok(message);
                }
                result => return result,
            }
        }
        unreachable!()
    }

    async fn stream(&self, request: &reqwest::RequestBuilder) -> Result<Value> {
        let (mut response, sent) = self.send(request).await?;
        let mut served = json!({"status": response.status().as_u16(), "headers": headers(response.headers())});
        let kind = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default();
        let kind = kind.to_lowercase();
        if kind.contains("html") || kind.contains("json") {
            bail!("the provider did not send an event stream: {kind}: {}", head(response).await);
        }
        let (mut lines, mut reply) = (Lines::default(), Reply::default());
        let (mut broken, mut eof, mut got, mut stalled, mut rejected) = (None, false, 0, false, false);
        let cap = tokio::time::Instant::from_std(sent) + TOTAL;
        let mut idle = tokio::time::Instant::now() + IDLE;
        'read: while !eof {
            match tokio::time::timeout_at(idle.min(cap), response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    got += chunk.len();
                    lines.push(&chunk);
                }
                Ok(Ok(None)) => {
                    eof = true;
                    lines.push(b"\n");
                }
                Ok(Err(e)) => {
                    broken = Some(chain(e));
                    break;
                }
                Err(_) => {
                    stalled = true;
                    broken = Some(if idle < cap {
                        format!("no output for {}s", IDLE.as_secs())
                    } else {
                        format!("the reply ran past {} minutes", TOTAL.as_secs() / 60)
                    });
                    break;
                }
            }
            while let Some(line) = lines.next() {
                let Some(data) = line.trim().strip_prefix("data:") else { continue };
                let data = data.trim();
                reply.finished |= data == "[DONE]";
                let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
                reply.events += 1;
                if let Some(error) = event.get("error").filter(|e| !e.is_null()) {
                    rejected = refused(error);
                    broken = Some(format!("provider error: {}", clip(&crate::redact::redact(&error.to_string()).0)));
                    break 'read;
                }
                for (key, value) in event.as_object().into_iter().flatten() {
                    if !CHUNK.contains(&key.as_str()) && !value.is_null() {
                        served[key.as_str()] = value.clone();
                    }
                }
                if reply.take(&event) {
                    idle = tokio::time::Instant::now() + IDLE;
                }
                std::io::stdout().flush()?;
            }
            if lines.partial() > LINE_MAX {
                rejected = true;
                broken = Some(format!("a stream line passed {} MB", LINE_MAX >> 20));
                break;
            }
        }
        (served["bytes"], served["events"]) = (got.into(), reply.events.into());
        let failed: Option<anyhow::Error> = match broken.as_deref() {
            Some(problem) if overflow(problem) => Some(ContextOverflow::new(problem.into()).into()),
            Some(problem) if rejected => Some(anyhow!("{problem}")),
            None if reply.events == 0 && !reply.finished => {
                Some(anyhow!("the provider did not send an event stream: {got} bytes without a single event"))
            }
            _ => None,
        };
        if let Some(e) = failed {
            reply.shown.cut();
            return Err(e);
        }
        if reply.finished {
            self.marked.set(true);
        }
        let accepted = !reply.finished
            && broken.is_none()
            && !self.marked.get()
            && !(reply.text.is_empty() && reply.calls.is_empty())
            && reply.calls.complete();
        if accepted {
            reply.reason = "eof".into();
        }
        if !reply.finished && !accepted {
            reply.shown.cut();
            let why = broken.unwrap_or_else(|| "the stream ended before the model finished".into());
            let mut lost = Lost::new(&reply.thinking, reply.began);
            lost.text = reply.text.chars().count() + reply.calls.size();
            lost.stalled = stalled;
            lost.response = served;
            return Err(Interrupted(why, lost).into());
        }
        reply.shown.done();
        if let Some(field) = reply.via.or(reply.echo) {
            self.field.set(field);
        }
        Ok(reply.message(sent, served))
    }

    async fn send(&self, request: &reqwest::RequestBuilder) -> Result<(reqwest::Response, Instant)> {
        let (mut timed_out, mut refused_key) = (false, false);
        for attempt in 1.. {
            let sent = Instant::now();
            // None when the builder holds an error: a malformed base_url, or an API key that is no valid header.
            let request = request
                .try_clone()
                .context("could not build the request: check the provider's base_url and API key")?;
            let limit = if timed_out { IDLE } else { FIRST_BYTE };
            let (problem, detail, wait) = match tokio::time::timeout(limit, request.send()).await {
                Ok(Ok(r)) if r.status().is_success() => return Ok((r, sent)),
                Ok(Ok(r)) => {
                    let (status, after) = (r.status(), retry_after(r.headers()));
                    let text = head(r).await;
                    let key = status == 400 && text.contains(CACHE_KEY);
                    let retryable = status == 429 || status.is_server_error() || key && !refused_key;
                    refused_key |= key;
                    if status == 413 || !retryable && overflow(&text) {
                        return Err(ContextOverflow::new(format!("{status}: {text}")).into());
                    }
                    if status == 402 || EXHAUSTED.iter().any(|e| text.to_lowercase().contains(e)) {
                        return Err(Exhausted(format!("{status}: {text}")).into());
                    }
                    if !retryable {
                        bail!("{status}: {text}")
                    }
                    (status.as_u16().to_string(), format!("{status}: {text}"), after)
                }
                Ok(Err(e)) => {
                    let problem = chain(e);
                    (problem.clone(), problem, None)
                }
                Err(_) if timed_out => {
                    let (first, second) = (FIRST_BYTE.as_secs(), IDLE.as_secs());
                    bail!("the provider stalled: no response within {first}s, then within {second}s");
                }
                Err(_) => {
                    timed_out = true;
                    let problem = format!("no response within {}s", FIRST_BYTE.as_secs());
                    (problem.clone(), problem, Some(1))
                }
            };
            if attempt == 5 {
                bail!("giving up after {attempt} attempts: {detail}")
            }
            let wait = wait.unwrap_or(2 << attempt).clamp(1, 60);
            let jitter = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            let extra = json!({"status": problem, "attempt": attempt, "wait_s": wait});
            crate::ui::event("retry", &format!("{problem}; retrying"), extra);
            tokio::time::sleep(Duration::from_secs(wait) + Duration::from_millis(jitter.subsec_millis() as u64)).await;
        }
        unreachable!()
    }
}

fn chain(e: reqwest::Error) -> String {
    format!("{:#}", anyhow::Error::from(e))
}

fn clip(text: &str) -> String {
    let text: String = text.trim().chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    match text.char_indices().nth(BODY_MAX) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text,
    }
}

async fn head(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    let read = async {
        while body.len() < BODY_MAX * 4
            && let Ok(Some(chunk)) = response.chunk().await
        {
            body.extend_from_slice(&chunk);
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(10), read).await;
    clip(&crate::redact::redact(&String::from_utf8_lossy(&body)).0)
}

fn refused(error: &Value) -> bool {
    let code = error["code"].as_u64().or(error["status"].as_u64());
    let named = format!("{} {}", error["type"], error["code"]).to_lowercase();
    code.is_some_and(|c| (400..500).contains(&c) && c != 408 && c != 429) || REJECTED.iter().any(|r| named.contains(r))
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers.get("retry-after")?.to_str().ok()?.trim().parse().ok()
}

/// The response body as lines, each handed over once it ends; every data line is one event. A line still arriving
/// waits, and is not scanned again.
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
    start: usize,
    scan: usize,
}

impl Lines {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.drain(..self.start);
        self.scan -= self.start;
        self.start = 0;
        self.buf.extend_from_slice(chunk);
    }

    fn next(&mut self) -> Option<String> {
        let Some(end) = self.buf[self.scan..].iter().position(|&b| b == b'\n' || b == b'\r') else {
            self.scan = self.buf.len();
            return None;
        };
        let end = self.scan + end;
        let line = String::from_utf8_lossy(&self.buf[self.start..end]).into_owned();
        (self.start, self.scan) = (end + 1, end + 1);
        Some(line)
    }

    /// The bytes of the line still arriving.
    fn partial(&self) -> usize {
        self.buf.len() - self.start
    }
}

/// A reply as it streams in: what it has said so far, when, and how much of it the terminal shows.
#[derive(Default)]
struct Reply {
    text: String,
    thinking: String,
    /// The field this provider sends reasoning in, once it has sent some.
    via: Option<&'static str>,
    /// The reasoning field the stream carried, if only empty. The reply goes back with it, since a DeepSeek upstream of
    /// OpenCode Go refuses a tool-call reply without it in the history with a 400, and it names the field from then
    /// on, so the empty one is sent under the same name in every request.
    echo: Option<&'static str>,
    calls: Calls,
    usage: Value,
    finished: bool,
    reason: Value,
    events: usize,
    /// The first token of any kind, of reasoning, and of text or a tool call.
    first: Option<Instant>,
    began: Option<Instant>,
    spoke: Option<Instant>,
    shown: Shown,
}

impl Reply {
    /// Takes in one event and shows what it adds; true when it carried anything, so the stream is not idle.
    fn take(&mut self, event: &Value) -> bool {
        if event["usage"].is_object() {
            self.usage = event["usage"].clone();
        }
        let why = event["choices"][0]["finish_reason"].as_str().filter(|w| !w.is_empty());
        if let Some(why) = why {
            (self.finished, self.reason) = (true, why.into());
        }
        let delta = &event["choices"][0]["delta"];
        let content = match &delta["content"] {
            Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
            content => content.as_str().unwrap_or_default().to_string(),
        };
        let field = FIELDS.into_iter().find(|f| delta[*f].as_str().is_some_and(|t| !t.is_empty()));
        self.echo = self.echo.or(FIELDS.into_iter().find(|f| delta[*f].is_string()));
        let tools = delta["tool_calls"].as_array().filter(|c| !c.is_empty());
        let reasoning = field.and_then(|f| delta[f].as_str()).unwrap_or_default();
        if !reasoning.is_empty() {
            self.via = self.via.or(field);
            self.thinking += reasoning;
            let began = *self.began.get_or_insert_with(Instant::now);
            self.first.get_or_insert(began);
            if self.text.is_empty() {
                crate::ui::thinking(&self.thinking, began.elapsed().as_secs());
            }
        }
        if delta["tool_calls"].is_array() || !content.is_empty() {
            self.spoke.get_or_insert_with(Instant::now);
            self.first.get_or_insert_with(Instant::now);
        }
        if !content.is_empty() {
            self.text += &content;
            self.shown.text(&content);
        }
        if delta["tool_calls"].is_array() {
            self.shown.calls(&self.thinking);
        }
        for call in tools.into_iter().flatten() {
            if self.calls.add(call) {
                crate::ui::writing(self.calls.lines + self.calls.list.len());
            }
        }
        why.is_some() || field.is_some() || tools.is_some() || !content.is_empty()
    }

    /// The finished reply as the session logs it, with its usage, timing and how it was served in meta.
    fn message(self, sent: Instant, served: Value) -> Value {
        let end = Instant::now();
        let ms = |a: Instant, b: Option<Instant>| b.map(|b| b.duration_since(a).as_millis() as u64);
        // Output tokens (reasoning included) per second, from the first token to the end of the stream.
        let speed = |(first, out): (Instant, u64)| (out as f64 / (end - first).as_secs_f64() * 10.0).round() / 10.0;
        let timing = json!({
            "ttft_ms": ms(sent, self.first), "think_ms": self.began.and_then(|b| ms(b, self.spoke.or(Some(end)))),
            "stream_ms": ms(sent, Some(end)), "tps": self.first.zip(self.usage["completion_tokens"].as_u64()).map(speed),
        });
        let meta = json!({"usage": self.usage, "timing": timing, "finish": self.reason, "response": served});
        let mut message = json!({"role": "assistant", "content": self.text, "meta": meta});
        if !self.thinking.is_empty() || self.echo.is_some() {
            message["reasoning_content"] = self.thinking.into();
        }
        if let Some(field) = self.via.or(self.echo) {
            message["meta"]["reasoning_field"] = field.into();
        }
        if !self.calls.is_empty() {
            message["tool_calls"] = self.calls.into_json();
        }
        message
    }
}

/// How a reply's text reaches the terminal. Text before any tool call is held: a short preface becomes the call's
/// intent line, while a long one, or one that opens like markdown, is the answer and streams as it arrives.
#[derive(Default)]
struct Shown {
    markdown: crate::render::Markdown,
    held: String,
    answering: bool,
    narrated: bool,
}

impl Shown {
    fn text(&mut self, content: &str) {
        if self.answering {
            self.markdown.push(content);
        } else if !self.narrated {
            self.held += content;
            crate::ui::activity("writing");
            let opens = self.held.trim_start().starts_with(['#', '-', '|']);
            if self.held.len() > 400 || self.held.contains("\n\n") || opens {
                crate::ui::prose();
                self.markdown.push(&std::mem::take(&mut self.held));
                self.answering = true;
            }
        }
    }

    /// Tool calls began: held text is their intent.
    fn calls(&mut self, thinking: &str) {
        if !self.answering && !self.narrated {
            self.narrated = true;
            crate::ui::intent(&self.held, thinking);
        }
    }

    /// The reply failed: an answer that was streaming ends where it stopped.
    fn cut(&mut self) {
        if self.answering {
            self.markdown.finish();
        }
    }

    /// The reply is complete: the answer ends, or held text that never became one is shown as the answer.
    fn done(&mut self) {
        if self.answering {
            self.markdown.finish();
        } else if !self.narrated && !self.held.trim().is_empty() {
            crate::ui::prose();
            self.markdown.push(&self.held);
            self.markdown.finish();
        }
    }
}

/// Tool calls assembled from their streamed pieces: id, name and arguments, keyed by the index the provider gives.
#[derive(Default)]
struct Calls {
    list: Vec<[String; 3]>,
    slots: HashMap<Option<u64>, usize>,
    /// Lines of code written so far, for the live row.
    lines: usize,
}

impl Calls {
    /// Adds one streamed piece; true when it carried arguments.
    fn add(&mut self, call: &Value) -> bool {
        let (key, function) = (call["index"].as_u64(), &call["function"]);
        // A piece with a new id, or with no index, after a complete call starts the next one.
        let fresh = self.slots.get(&key).is_none_or(|&i| {
            let [id, name, args] = &self.list[i];
            let new_id = call["id"].as_str().is_some_and(|i| !i.is_empty() && !id.is_empty() && i != id);
            (key.is_none() || new_id)
                && function["name"].is_string()
                && !name.is_empty()
                && serde_json::from_str::<Value>(args).is_ok()
        });
        if fresh {
            self.list.push(Default::default());
            self.slots.insert(key, self.list.len() - 1);
        }
        let [id, name, args] = &mut self.list[self.slots[&key]];
        if let Some(given) = call["id"].as_str().filter(|_| id.is_empty()) {
            *id = given.into();
        }
        if let Some(part) = function["name"].as_str() {
            if part.starts_with(name.as_str()) {
                *name = part.into();
            } else {
                *name += part;
            }
        }
        let part = match &function["arguments"] {
            Value::String(part) => part.clone(),
            Value::Null => return false,
            part => part.to_string(),
        };
        *args += &part;
        self.lines += part.matches("\\n").count();
        true
    }

    fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Whether every call's arguments are whole JSON.
    fn complete(&self) -> bool {
        self.list.iter().all(|c| serde_json::from_str::<Value>(&c[2]).is_ok())
    }

    /// The bytes of arguments received.
    fn size(&self) -> usize {
        self.list.iter().map(|c| c[2].len()).sum()
    }

    /// The calls as the message carries them: arguments that are not whole JSON become {}, and a call without an id
    /// gets one.
    fn into_json(self) -> Value {
        let calls = self.list.into_iter().enumerate().map(|(i, [id, name, args])| {
            let args = if serde_json::from_str::<Value>(&args).is_ok() { args } else { "{}".into() };
            let id = if id.is_empty() { format!("call_{i}") } else { id };
            json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}})
        });
        calls.collect()
    }
}
