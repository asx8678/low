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
changes   calc.py +1 −1
undo      git restore --source=790cc5e2fb18 --worktree -- calc.py  (low's edit()/write() paths; new files stay)
cost      3 requests · 2 runs · 4.9k tok in · 266 tok out · 0:10
```

## Why low

- **Code mode.** The model writes a program instead of making many small tool calls: fewer round trips, fewer tokens.
- **Sandboxed by default.** No network (localhost only, on macOS), writes only inside the repo and temp dirs, and your SSH keys, cloud credentials and shell history are out of reach.
- **Verified.** low runs your project's check on the final tree, and every turn ends with a verdict.
- **Reversible.** Inside a git repository, low never commits to your branches: git is the undo, and every turn report prints the exact restore command. The snapshots those commands restore stay under `refs/low/snapshots` for 14 days, so `git gc` can't remove them; `git update-ref -d refs/low/snapshots` drops them sooner. A file git ignores, such as `.env`, is listed as changed but can't be restored that way, and the report says so. For the same reason `sh()` refuses `git clean -x` and `-X`, which delete every ignored file, and so does `subprocess` in a run.
- **Low entropy.** A short system prompt, deterministic harness decisions, rule-based context compaction, and no hidden LLM calls: the one low makes beyond the turn's, a second look at an unchecked turn's claims when the model asks for one, shows as it runs and counts in the turn's cost.
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

`check` is one shell command, so `&&` chains several. In a monorepo whose root build doesn't run the tests of the package a turn changes, add the tests your runner relates to the changed files after a build of the whole tree, e.g. `npx tsc --noEmit && f=$({ git diff --name-only HEAD; git ls-files -o --exclude-standard; } | grep '\.ts$'); [ -z "$f" ] || npx vitest related --run $f`. A check that only builds still makes a turn VERIFIED, and a verified turn gets no second look; with `check = ""` it is NO CHECK and gets one when the model asks with `look()`.

Without a `check`, a new session works one out from the files at the repository's root: a Makefile's `check` or `test` target, then the first of `Cargo.toml` (`cargo build -q && cargo test -q`, with `--workspace` in a workspace that sets no `default-members`), `package.json` (its `check` or `typecheck` script and its `test` script, with the lockfile's package manager), a pytest setup, `go.mod` (`go build ./... && go test ./...`) and `mix.exs` (`mix compile --warnings-as-errors && mix test`), each followed by its build alone. Before the first request it runs them in that order, in the sandbox and for at most 5 minutes each, on the tree as it finds it, and uses the first that passes, so tests that already fail don't hold every turn's verdict, and a repository whose tests need a database still gets its build checked. If none passes, the session has no check. A resumed session keeps what its start decided. Lows started together in one tree wait for one such run and reuse its result for up to an hour. `check = ""` turns this off.

**3. Give it a task:**

```sh
low "the median of an even-length list is wrong; fix it"
```

## Usage

| command | does |
|---|---|
| `low "<task>"` | runs one task and exits: 0 done, 1 not verified (the check, low.toml's or one low detected, didn't pass on the final tree), 3 an error, 4 the model asked a question (answer it as the next task, e.g. `low --resume "<answer>"`), 5 stopped short |
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
| `/models [filter]` | picks the model and the reasoning effort in one panel: ↑↓ the model, ←→ the effort (`default` is the model profile's), typing filters, Enter applies, Esc keeps both; a name no provider lists can be typed and tried. Without a terminal it is a numbered list |
| `/model [filter]` | the same as `/models` |
| `/effort [level]` | shows the reasoning effort the session sends, or sets it: `low`, `medium`, `high`, `xhigh` or `max` |
| `/providers` | adds, edits and removes providers |
| `/cost` | shows the session's requests and tokens |
| `/think` | shows the model's last reasoning |
| `/compact` | compacts the context now |

While a turn runs, what you type shows on the row above the status bar, and Enter sends it to the model with its next request, once the runs in flight have their results: it steers the turn without stopping it. A paste stays whole: Enter sends its lines as one message, with the indentation and blank lines inside it, and the row shows its line breaks as ⏎. ↑ takes back the last line while no request has taken it. What the turn doesn't take (a line sent as the model writes its final reply, or a command such as `/cost`) runs when the turn ends, and a line not yet sent starts the next prompt. Ctrl-C still stops the turn, and puts what wasn't taken back at the prompt instead of running it.

## Configuration

| file | holds |
|---|---|
| `~/.config/low/providers.toml` | endpoints: `base_url`, optional `api_key`, `models` |
| `~/.config/low/config.toml` | your defaults: `model`, extra sandbox `env`, and per-model request parameters |
| `low.toml` (repo root) | `check`, `fmt` and `protect` for that repository (config.toml refuses them, since they belong to one repository) |
| `AGENTS.md` (repo root) | instructions for the model, which low puts in its system prompt (the first 32 KB); `CLAUDE.md` when there is no `AGENTS.md` |
| `AGENTS.md` (a folder) | instructions for the files in that folder: the first time a run shows, edits or writes a file under it, the worker prints them (the first 8 KB), outermost folder first, and not again while the context still holds them; `CLAUDE.md` when the folder has no `AGENTS.md` |

A `config.toml` example:

```toml
model = "deepseek-v4.1-flash"        # default model
env = ["MY_VAR"]                     # environment variables passed into the sandbox
search = "deepseek"                  # the provider whose key search() and fetch() use (default: one named deepseek)

[models."deepseek-v4.1-flash"]       # sent with every request to this model
temperature = 0.0                    # temperature = false sends none, for a model that refuses it
reasoning_effort = "low"             # the default effort; --effort, /effort and /models change it per session
compact_at = 200000                  # compact the context at this many prompt tokens
keep_warm = 600                      # low --serve: re-send the prompt after this many idle seconds, for an hour after the last task (default 600, 0: never)
turn_budget = 60                     # stop a turn after this many compact_at of tokens (cached input counts a quarter; default 60);
                                     # the model is told at three quarters, and asked for a handoff at the stop
session_budget = 500                 # stop the session's turns once it has spent this many compact_at (e.g. a long --tasks run; default none)
fallback = "zro:zro/deepseek-v4.1-flash"  # when this provider is out of credit or quota (402), go on with this one
```

## How it works

Inside `run(code)`, the model has a few helpers:

| helper | does |
|---|---|
| `sh(cmd)` | runs a shell command; returns stdout and stderr, with `[exit N]` on failure. A command the run's code starts with `subprocess` or `os.system` instead is listed and refused as `sh()`'s are. A run's own code stops after 3 minutes, so a loop stuck on one case costs that much; `sh(cmd, timeout=N)` gives one command N seconds (120 by default), and a run up to 9½ minutes |
| `show(path, start, end)` | prints numbered lines, paging long files; `show(path, regex)` prints each `def`/`fn`/`class` block with a line that matches, so finding code and reading it take one call |
| `recall(n, start, end)` | prints entry `#n` of the session, one a compaction's summary cites but replaced, as it was: a message, or a run's code, its result and the model's reasoning; it pages like `show`. low reads it from the session log, which the sandbox hides, and makes no LLM call; entries the context still holds are refused |
| `edit(path, old, new)` | replaces exactly one occurrence, or several pairs all-or-none, then runs `fmt`; an `old` whose lines are all indented too deep or too shallow by the same amount still matches when that finds one place, and `new` is indented the same way |
| `write(path, content)` | creates a file, or replaces one |
| `undo(*paths)` | puts files back as they were when the turn began, from its snapshot (by default every file the turn's `edit` and `write` changed), and removes the ones `write` created; inside a git repository only, since the sandbox blocks `git checkout` and `restore` |
| `check()` | runs the repository's `check` (low.toml's, or the one low detected) inside the sandbox; with none it runs nothing and returns `True` |
| `accept(criteria)` · `plan(steps)` · `done()` | record what done means and track the turn's steps: shown as the turn runs, and in a `--json` result |
| `finish(report)` · `ask(question)` | end the turn with the final report, or with a question for you that the next task answers |
| `look(why)` | asks for a second look at the report once the turn ends (below), saying what the model is unsure of; it runs only when the turn changed files and no check passed |
| `search(query)` · `fetch(url)` | web search and a page as markdown (JSON, code and other text files as their text, less some blank lines but for the open hosts below), through [DeepSeek's search API](https://api-docs.deepseek.com/guides/search/); low makes the request, since the sandbox has no network. A list of up to 50 queries or URLs goes out at once, 10 requests at a time, and comes back as a list in the same order, with the error in place of each one that failed. Only when a provider is set up for it (below), except `fetch` on the open hosts. `fetch` takes a URL that a `search` returned or the task gives, or any https URL on a package registry, CDN or docs host that shows no one the URLs asked of it (npm, PyPI, crates.io, docs.rs, GitHub's API and raw files, jsDelivr, unpkg, cdnjs, and the Rust, Python and MDN docs; jsDelivr's public hit counts per file are the nearest thing). low fetches those hosts' URLs itself, from your IP as your package manager does, and gives them back as served, with no provider needed; it follows redirects only among them, refuses binary files and bodies over 8 MB, and sends an HTML page, a rate limit, a server error or a redirect elsewhere on to DeepSeek when a provider is set up. Both refuse an argument that looks like a secret, so code in a run can't send what it read to someone who would see it |

Repository files change only through `edit`, `write` and `undo`, so low knows exactly what the model touched. A turn that changed files without them is asked once to clean up its scratch output.

Every turn ends with one of five verdicts:

| verdict | meaning |
|---|---|
| **VERIFIED** | files changed, and the check passed on the final tree |
| **UNVERIFIED** | files changed, but the check didn't pass, or the turn changed what the check runs |
| **NO CHANGES** | nothing changed, e.g. a question or a review |
| **NO CHECK** | files changed, and there is no check: low.toml sets none and low detected none that passes on the tree as found, or `check = ""` |
| **QUESTION** | the model asked you something (exit 4); answer it as the next task |

**A second look.** A turn that changed files with no check passing (none configured, or one still failing after low asked the model to fix it) gets a second look at its report before it ends, when the model asked for one with `look(why)`: low leaves the call to the model, so a turn that doesn't ask costs no extra requests. The same model, in a conversation of its own that starts with the turn's system prompt, so the provider's prompt cache still has it, gets the task, the criteria, the model's reasons, what each run edited and ran, the diff and the final report, but none of the turn's messages. It picks the 3 to 5 claims likeliest to be wrong and checks them by reading the code, in a worker whose sandbox keeps the repository read-only: `edit()` and `write()` refuse, and `check()`, `search()` and `fetch()` are off. It is told not to build or run the project; `sh()` still works, so that rests on the prompt and the read-only tree (under `--yolo`, on the prompt and `edit()` and `write()` alone). A claim the code contradicts goes back to the model once, and a second look at its answer checks those claims alone, with the runs and the diff since, in at most 3 requests; the others stand. The report's `claims` row says how many held, were contradicted or stayed uncertain; the verdict and exit status stay as the table says. A look makes at most 6 requests, the last told to give its verdict; they show as it runs, are logged as `aside` events, and count in the turn's cost and in `/cost`.

**The sandbox:**
- **Network:** off. On macOS only localhost is reachable; on Linux, only servers started inside the sandbox. `search()` and `fetch()` send their query or URL out through low (above).
- **Writes:** only the repository and temp directories. `.git` and the hooks directory are off limits. A git repository the model makes inside the tree is reported, since your git would run hooks and config from there outside the sandbox; `git config --global safe.bareRepository explicit` makes git refuse one laid out as a bare repository.
- **Reads:** credentials and private data in your home directory are hidden, in your account's home too when `HOME` names another folder: `~/.ssh` and `~/.gnupg`; cloud, Kubernetes and Docker configs; package-registry and database credentials (`.npmrc`, `.pypirc`, `.pgpass`, `.my.cnf`, …); keychains and keyrings; shell and REPL histories; the profiles of Chrome, Firefox, Safari, Brave, Edge, Chromium and Arc; other coding agents' keys and data (all of `~/.claude`); and low's own config.
- **Environment:** only an allowlist of variables is passed in, plus your `env = [...]`. On macOS any process of yours can read the environment another one started with, so low restarts itself at launch with only those variables and hands `LOW_API_KEY` and the proxy variables (`HTTPS_PROXY` and the rest, which can hold a password) to the new process through a pipe. Your `env = [...]` values stay readable by the model's code. Other processes stay readable, so a shell or agent that starts low with secrets in its own environment still exposes them to the model's code. On Linux the sandbox has its own process namespace, so the model's code sees no process outside it.
- **Redaction:** secrets that appear in output, such as API keys, tokens, private keys, URL passwords and AWS secret keys, are redacted before the model or the session log sees them. It matches known shapes, so a secret that is encoded, split or has no recognizable prefix passes through: redaction is a safety net, and the sandbox is what keeps your home directory's secrets out of reach.

**Outside a git repository** low still runs, but it can't undo: the turn report lists what `edit` and `write` changed and says so. In your home directory, or above it, low can read files but not change them (your account's home counts even when `HOME` names another folder), because writes there could reach shell startup files and launch agents outside the sandbox.

**Sessions** are JSONL files in `~/.low/sessions`, readable only by you. Run and check output is redacted there, but your tasks and steers are kept as you typed them, so a key you paste into one stays in the log. Each task the model gets ends with the date it was given on, in your time zone, so the system prompt stays the same and cached. When the context reaches `compact_at`, low compacts it without an LLM call: it keeps the task, the latest messages and a summary rebuilt from the log. The summary cites what it replaced as entries (`#412`), and `recall(412)` opens one as it was; the summary says so too, so a session started before `recall` existed learns of it at its next compaction.

**Requests** carry the session's id in an `x-session-affinity` header. A gateway that spreads a model over several replicas can route by it, so the session keeps reaching the one that has its prompt cached; a server that doesn't know the header ignores it. A reply whose reasoning goes round in a loop, the same text over and over, is cut off as it streams, once most of its last 16 KB repeats, instead of running on to the provider's output limit (64k tokens and about six minutes with DeepSeek V4.1 Flash). The model is told why and asked for its next step, and a third unusable reply in a row stops the turn.

**Turn limits:** a turn stops at its token budget (`turn_budget`) or after 200 requests. Once it has used three quarters of either, the model is told about how many requests it has left and asked to spend them on the change rather than on reading; it is not asked to stop early, so a `--tasks` task cut off while it is still changing files keeps its extra turns. At the stop, low asks the model once more, running no code, where the work stands: what changed, what is left and the next step. That reply is the turn's answer, and the next turn starts from it. The session's budget (`session_budget`) caps spending, so it stops without asking.

## Development

`low.toml`'s check runs rustfmt, the build, clippy and the unit tests. The end-to-end tests drive the built binary against a scripted provider in a throwaway HOME and repository: `cargo build && python3 tests/e2e/run.py`. `--slow` adds the tests that take minutes, and `LOW_BIN` picks another binary. The check can't run them, because a sandboxed check would start sandboxed lows inside it and macOS's `sandbox-exec` doesn't nest.

## License

[MIT](https://github.com/asx8678/low/blob/main/LICENSE)
