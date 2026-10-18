# low

**A low-entropy, code-mode coding agent for your terminal.**

[![crates.io](https://img.shields.io/crates/v/low.svg)](https://crates.io/crates/low)
[![downloads](https://img.shields.io/crates/d/low.svg)](https://crates.io/crates/low)
[![license: MIT](https://img.shields.io/crates/l/low.svg)](https://github.com/asx8678/low/blob/main/LICENSE)
![platforms: macOS | Linux](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-informational)
![rust 1.99+](https://img.shields.io/badge/rust-1.99%2B-orange)

low gives the model one tool, `run(code)`. Each step is a short Python script that runs in a persistent, sandboxed worker at the root of your project. Reading files, editing them and running tests all happen inside that script, so a step that takes many tool calls in other agents is usually one round trip here.

```text
$ low "add() returns the wrong result; fix it"
  ▸ I'll look at the files first.
  ◆ criteria add(2, 3) == 5; add returns a + b; the project check passes
  ✓ edit calc.py  +1 −1
  ✓ check passed

Fixed `calc.py:2` — `add` subtracted instead of adding (`return a - b` → `return a + b`).

Verified: the project check (`python3 -m unittest -q`) passes, and direct calls give
`add(2,3)==5`, `add(-4,1)==-3`, `add(0,0)==0`. Nothing open.

── VERIFIED ────────────────────────────────────
task      add() returns the wrong result; fix it
criteria  add(2, 3) == 5; add returns a + b; the project check passes
check     passed on the final tree
changes   calc.py +1 −1
undo      git restore --source=790cc5e2fb18 --worktree -- calc.py  (low's edit()/write() paths; new files stay)
cost      3 requests · 2 runs · 4.9k tok in · 266 tok out · 0:10
```

## Why low

- **Code mode.** The model writes a program instead of making many small tool calls: fewer round trips, fewer tokens.
- **Sandboxed by default.** No network, writes only inside the repo and temp dirs, and your SSH keys, cloud credentials and shell history are out of reach.
- **Verified.** low runs your project's check on the final tree, and every turn ends with a verdict.
- **Reversible.** Inside a git repository, low never commits to your branches: git is the undo, and every turn report prints the exact restore command. The snapshots those commands restore stay under `refs/low/snapshots` for 14 days, so `git gc` can't remove them; `git update-ref -d refs/low/snapshots` drops them sooner. A file git ignores, such as `.env`, is listed as changed but can't be restored that way, and the report says so. For the same reason `sh()` refuses `git clean -x` and `-X`, which delete every ignored file.
- **Low entropy.** A short system prompt, deterministic harness decisions, rule-based context compaction, and no hidden LLM calls.
- **Any OpenAI-compatible model.** Point it at any `/chat/completions` endpoint.

In a [15-task benchmark](https://github.com/asx8678/low/blob/7d9ab8a/tuning/REPORT.md) with the same model (DeepSeek V4.1 Flash), OpenCode needed about 1.3× low's round trips, 2.5× its tool calls and 2× its uncached input tokens.

## Install

```sh
cargo install low
```

| you need | why |
|---|---|
| macOS or Linux | the sandbox is `sandbox-exec` on macOS and [bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`) on Linux |
| `git` | inside a repository, low tracks what it changes and prints an undo; elsewhere it runs without undo |
| Python 3 | the worker runs on the active virtualenv's Python, or else the newest `python3.N` on your `PATH` |

## Quick start

**1. Add a provider** in `~/.config/low/providers.toml`:

```toml
[opencode]
base_url = "https://opencode.ai/inference/go/v1"
api_key = "…"                       # optional: else $LOW_API_KEY, else ~/.config/low/api_key
models = ["deepseek-v4.1-flash"]
```

**2. Tell low how to check your repository** with a `low.toml` at its root:

```toml
check = "cargo build -q && cargo test -q"   # run on the final tree; decides the verdict
fmt = "rustfmt --edition 2024"              # run on each edited file (its path is appended)
protect = ["Cargo.lock", "migrations"]      # files and folders the model may not change (also --protect <path> per run;
                                            # the sandbox enforces it, so under --yolo only edit()/write() refuse them)
```

**3. Give it a task:**

```sh
low "the median of an even-length list is wrong; fix it"
```

## Usage

| command | does |
|---|---|
| `low "<task>"` | runs one task and exits: 0 done, 1 not verified (a configured check didn't pass on the final tree), 3 an error, 4 the model asked a question (answer it as the next task, e.g. `low --resume "<answer>"`), 5 stopped short |
| `low` | starts the REPL |
| `low --resume` | continues the latest session in this repository |
| `low --session <id>` | continues that session of this repository |
| `low --tasks <file>` | works through the open tasks of a markdown checklist (`- [ ] task`) in one session, marking each `[x]` done, `[!]` not verified or `[?]` asked a question, with its result under it; an `  - answer: …` line under a `[?]` task is sent next; text above the first task goes with the first task of each run; the model can read the checklist but not change it; a task cut off by a turn limit while it was still changing files gets up to 2 more turns; otherwise stops at the first task not done |
| `low --json "<task>"` | shows nothing while the task runs and prints its result as one JSON object (session, verdict, status, exit, answer, changes, check, tokens) |
| `low -m <model>` | picks the model for a new session; with `--resume`, switches the session to it. `provider:model` picks the provider too, for an id several providers list |
| `low --effort <level>` | how hard the model thinks: `low`, `medium`, `high`, `xhigh` or `max`, sent as `reasoning_effort`; like `-m`, it sets a new session and switches a resumed one. DeepSeek runs `medium` and `xhigh` as `high`, and caches the prompt per level, so a switch mid-session sends the whole context uncached once |
| `low --yolo` | turns the sandbox off: network on, writes anywhere (environment variables stay filtered) |
| `low --serve ["<task>"]` | keeps the session open after the task and takes the next ones from `low --send`; while it waits, it re-sends the prompt so the provider keeps it cached (`keep_warm`) |
| `low --send "<task>"` | gives the task to the `low --serve` running in this repository and prints its result, with the same exit status as `low "<task>"`; it waits as long as the task runs, and a `timeout` around it stops only the waiting; `low --send /stop` ends the server |

In the REPL:

| command | does |
|---|---|
| `/model [filter]` | lists the models your providers serve and switches to one |
| `/effort [level]` | shows the reasoning effort the session sends, or sets it: `low`, `medium`, `high`, `xhigh` or `max` |
| `/providers` | adds, edits and removes providers |
| `/cost` | shows the session's requests and tokens |
| `/think` | shows the model's last reasoning |
| `/compact` | compacts the context now |

## Configuration

| file | holds |
|---|---|
| `~/.config/low/providers.toml` | endpoints: `base_url`, optional `api_key`, `models` |
| `~/.config/low/config.toml` | your defaults: `model`, extra sandbox `env`, and per-model request parameters |
| `low.toml` (repo root) | `check`, `fmt` and `protect` for that repository (config.toml refuses them, since they belong to one repository) |

A `config.toml` example:

```toml
model = "deepseek-v4.1-flash"        # default model
env = ["MY_VAR"]                     # environment variables passed into the sandbox
search = "deepseek"                  # the provider whose key search() and fetch() use (default: one named deepseek)

[models."deepseek-v4.1-flash"]       # sent with every request to this model
temperature = 0.0                    # temperature = false sends none, for a model that refuses it
reasoning_effort = "low"             # the default effort; --effort and /effort change it per session
compact_at = 200000                  # compact the context at this many prompt tokens
keep_warm = 600                      # low --serve: re-send the prompt after this many idle seconds, for an hour after the last task (default 600, 0: never)
turn_budget = 60                     # stop a turn after this many compact_at of tokens (cached input counts a quarter; default 60)
session_budget = 500                 # stop the session's turns once it has spent this many compact_at (e.g. a long --tasks run; default none)
fallback = "zro:zro/deepseek-v4.1-flash"  # when this provider is out of credit or quota (402), go on with this one
```

## How it works

Inside `run(code)`, the model has a few helpers:

| helper | does |
|---|---|
| `sh(cmd)` | runs a shell command; returns stdout and stderr, with `[exit N]` on failure |
| `show(path, start, end)` | prints numbered lines, paging long files |
| `edit(path, old, new)` | replaces exactly one occurrence, or several pairs all-or-none, then runs `fmt` |
| `write(path, content)` | creates a file, or replaces one |
| `check()` | runs the repository's `check` inside the sandbox; with none configured it runs nothing and returns `True` |
| `accept(criteria)` · `plan(steps)` · `done()` | record what done means and track progress for the turn report |
| `finish(report)` · `ask(question)` | end the turn with the final report, or with a question for you that the next task answers |
| `search(query)` · `fetch(url)` | web search and a page as markdown, through [DeepSeek's search API](https://api-docs.deepseek.com/guides/search/); low makes the request, since the sandbox has no network. Only when a provider is set up for it (below). `fetch` takes only a URL that a `search` returned or the task gives, and both refuse an argument that looks like a secret, so code in a run can't send what it read out through them |

Repository files change only through `edit` and `write`, so low knows exactly what the model touched. A turn that changed files without using either is asked once to clean up its scratch output.

Every turn ends with one of four verdicts:

| verdict | meaning |
|---|---|
| **VERIFIED** | files changed, and the check passed on the final tree |
| **UNVERIFIED** | files changed, but the check didn't pass, or the turn changed what the check runs |
| **NO CHANGES** | nothing changed, e.g. a question or a review |
| **NO CHECK** | files changed, and no check is configured |
| **QUESTION** | the model asked you something (exit 4); answer it as the next task |

**The sandbox:**
- **Network:** off. On macOS only localhost is reachable; on Linux, only servers started inside the sandbox. `search()` and `fetch()`, when set up, send their query or URL out through low (above).
- **Writes:** only the repository and temp directories. `.git` and the hooks directory are off limits. A git repository the model makes inside the tree is reported, since your git would run hooks and config from there outside the sandbox; `git config --global safe.bareRepository explicit` makes git refuse one laid out as a bare repository.
- **Reads:** credentials and private data in your home directory are hidden: `~/.ssh` and `~/.gnupg`; cloud, Kubernetes and Docker configs; package-registry and database credentials (`.npmrc`, `.pypirc`, `.pgpass`, `.my.cnf`, …); keychains and keyrings; shell and REPL histories; the profiles of Chrome, Firefox, Safari, Brave, Edge, Chromium and Arc; other coding agents' keys; and low's own config.
- **Environment:** only an allowlist of variables is passed in, plus your `env = [...]`. On macOS any process of yours can read the environment another one started with, so low restarts itself at launch with only those variables and hands `LOW_API_KEY` to the new process through a pipe. Other processes stay readable, so a shell or agent that starts low with secrets in its own environment still exposes them to the model's code. On Linux the sandbox has its own process namespace, so the model's code sees no process outside it.
- **Redaction:** secrets that appear in output, such as API keys, tokens and private keys, are redacted before the model or the session log sees them.

**Outside a git repository** low still runs, but it can't undo: the turn report lists what `edit` and `write` changed and says so. In your home directory, or above it, low can read files but not change them, because writes there could reach shell startup files and launch agents outside the sandbox.

**Sessions** are JSONL files in `~/.low/sessions`. When the context reaches `compact_at`, low compacts it without an LLM call: it keeps the task, the latest messages and a summary rebuilt from the log.

## License

[MIT](https://github.com/asx8678/low/blob/main/LICENSE)
