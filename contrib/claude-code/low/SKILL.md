---
name: low
description: Delegate coding tasks to low, a sandboxed coding agent CLI, and steer it — one-shot runs with a JSON result, a warm session that takes task after task (low --serve / --send), markdown task lists (--tasks), and answering the questions low asks. Use when the user asks to have low do the work, to run tasks through low, or to keep a low session going.
---

# Driving low from Claude Code

low runs one coding task at a time in the current git repository, in a sandbox, and ends each task with a verdict. Drive it through Bash; always pass `--json` so the output is one JSON object instead of a live display.

## One task

```bash
low --json "Fix the off-by-one in parse_range() in src/range.py"
```

stdout is a single JSON object (stderr has a `[session …]` line):

```json
{"status": "done", "exit": 0, "verdict": "VERIFIED", "answer": "…low's final message…",
 "question": null, "criteria": "…what done means…", "check": "passed on the final tree",
 "changes": ["src/range.py +2 −1"], "undo": "git restore --source=… --worktree -- .",
 "requests": 3, "tokens": {"prompt": 5600, "cached": 4100, "out": 400}, "cost": null, "ms": 6100,
 "id": "1792263543-19173", "session": "/Users/…/.low/sessions/1792263543-19173.jsonl"}
```

| exit | status | meaning | what to do |
|---|---|---|---|
| 0 | done | VERIFIED, NO CHANGES or NO CHECK | read `answer` and `changes` |
| 1 | unverified | the repo's check (low.toml `check`) did not pass on the final tree | look at `check`, then send a follow-up task or undo |
| 2 | — | bad arguments | fix the command |
| 3 | error | provider or setup error; `error` has the text | retry later or change provider |
| 4 | question | low needs an answer; `question` has it | answer it as the next task (below) |
| 5 | stopped | interrupted or a limit hit | resume with a narrower task |

`changes` marks a file git ignores as `(ignored by git)`: it changed, but the `undo` command can't restore it. `repos` lists git repositories the task made inside the tree, which git would run hooks from outside the sandbox.

A task can outlast the 10-minute limit of a foreground Bash call: run long ones with `run_in_background` and read the JSON when notified. No JSON on stdout means low was killed; the session log still has the turn.

## Follow-ups and answers

Later tasks continue the same conversation (and the provider's prompt cache):

```bash
low --session 1792263543-19173 --json "Also cover the empty range"
low --resume --json "3000"          # the newest session of this repo, e.g. to answer exit-4 questions
```

## A warm session for many tasks

```bash
low --serve                          # run_in_background; it prints [session …] and waits
low --send --json "first task"       # blocks until done; same JSON and exit codes
low --send --json "next task"
low --send /stop                     # ends the server
```

While it waits, the server re-sends the conversation every `keep_warm` seconds (default 600) so the next task's prompt stays cached. Start the server without a task when scripting: a task given to `--serve` itself has no completion signal. `--send --session ID` targets one server when several run.

## Task lists

Write a markdown checklist and let low work through it in one session:

```markdown
- [ ] Add a --port option to the server
- [ ] Document it in README.md
```

```bash
low --tasks TASKS.md --json
```

low marks each line `[x]` done, `[!]` not verified or `[?]` asked a question, writes its result under it as `  - low: …`, records the session in the file's first line, and stops at the first task not done (a task cut off by a turn limit while it was still changing files first gets up to 2 more turns). Text above the first task, such as rules for all of them, goes to the model with the first task of each run. The model can read the checklist but not change it; low alone marks it. To answer a `[?]`, add `  - answer: …` under it and run the same command again. The user can read the file at any time; edits made while low runs are kept.

## Choosing the model

`-m provider:model` picks the provider when several list the same id, e.g. `low -m opendesign:deepseek-v4.1-flash --json "…"`; plain `-m model` takes the first provider that lists it. Providers live in `~/.config/low/providers.toml`. `--effort low|medium|high|xhigh|max` sets how hard the model thinks (DeepSeek runs medium and xhigh as high); with `--session` or `--resume` it switches the session.

## Web search

Inside a task, low's model can call `search(query)` and `fetch(url)` (DeepSeek's search API) when a `[deepseek]` provider with an API key is in providers.toml, or `search = "<provider>"` in `~/.config/low/config.toml` names one. Without it the helpers are not offered. `fetch(url)` takes only a URL that a `search()` returned or that the task text gives, so put a URL in the task when low should read a specific page.

## Rules

- Keep low's sandbox on: don't pass `--yolo` unless the user asks.
- One low per working tree at a time; for parallel tasks use separate git worktrees, or give each low its own paths and pass the others' with `--protect <path>` (repeatable), which the sandbox enforces. `--protect` and `--tasks` paths are relative to where low runs, like any shell path.
- Treat `answer` as low's claim and the verdict as evidence: VERIFIED means the repo's check passed on the final tree, not that the task is fully right. Review `changes` before telling the user it's done.
- Give low whole tasks with their acceptance criteria; it has its own tools and doesn't need step-by-step instructions.
