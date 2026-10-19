import ast, contextlib, ctypes, difflib, importlib.util, io, json, linecache, os, re, shlex, signal, stat, subprocess, sys, tempfile, threading, time, traceback

ROOT = os.getcwd()
sys.path.append(ROOT)
CHECK = os.environ.pop("LOW_CHECK", "")
FMT = os.environ.pop("LOW_FMT", "")
PROTECTED = [os.path.realpath(d) for d in json.loads(os.environ.pop("LOW_PROTECTED", "[]"))]
LOCKED = [os.path.realpath(d) for d in json.loads(os.environ.pop("LOW_LOCKED", "[]"))]
READONLY = os.environ.pop("LOW_READONLY", "")
HALF = 16000
LIMIT = 8 << 20
OUT_CAP = 64 << 20  # what a run's own printing may put on disk; past it, output is dropped
SPILLS = 10  # the cut runs' whole outputs kept on disk for show(); older ones are deleted
RUN_CAP = 570
GUIDES = ("AGENTS.md", "CLAUDE.md")  # a folder's instructions: the first of these it has
GUIDE_CAP = 8 << 10  # characters of one folder's instructions a run shows; show() reads the rest
DEFINITION = re.compile(r"\s*(?:(?:pub(?:\([^)]*\))?|async|export|default|static|public|private|protected|final|"
                        r"abstract|override|unsafe|extern|const|inline|virtual)\s+)*(?:def|class|fn|func|function|impl|"
                        r"struct|enum|trait|interface|module|mod|object|record|type)\b")
IGNORABLE = re.compile("[\u200c-\u200f\u202a-\u202e\u206a-\u206f\ufeff]")
ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\r(?!\n)")
ESCAPED = re.compile(r"\\([!-/:-@\[-`{-~])")  # markdown's backslash before ASCII punctuation, as DeepSeek's fetch writes it

_inbox = os.fdopen(os.dup(0), encoding="utf-8")
_outbox = os.fdopen(os.dup(1), "w", encoding="utf-8")
_null = os.open(os.devnull, os.O_RDWR)
os.dup2(_null, 0)
_report = {}
_state = {}
_checked = None
_said = 0  # bytes this run printed through edit, write and check, the output the model can predict
_turn = {}  # what this turn's edit(), write() and undo() changed and write() created, for undo(); seeded from low's log
_groups = set()
_spawned = []  # the commands this run's code started with subprocess, each with its Popen, for their exit status
_Popen, _os_system = subprocess.Popen, os.system  # low's own commands; the run's code gets _Seen and _system
_deadline = time.monotonic() + RUN_CAP
_running = _dropped = _executing = False
_flow = threading.Lock()  # held while a run's output is switched, so the watch never switches the wrong run's


def _overrun(signum, frame):
    # Code that never returns, a pure-Python loop say, ends here, before the host's timeout kills the worker and its
    # variables; sh() commands meet their own deadline first.
    if _executing:
        raise TimeoutError(f"the run went past its {RUN_CAP} s limit")


def _host(message):
    # The run's clock stops while the host answers (a check can take far longer than a run), and starts over after it,
    # as the host's own timeout does.
    global _deadline
    signal.setitimer(signal.ITIMER_REAL, 0)
    _outbox.write(json.dumps(message) + "\n")
    _outbox.flush()
    answer = json.loads(_inbox.readline() or "{}")
    _deadline = time.monotonic() + RUN_CAP
    if _executing:
        signal.setitimer(signal.ITIMER_REAL, RUN_CAP + 5)
    return answer


def _children(pid):
    if sys.platform == "darwin":
        buf = (ctypes.c_int * 4096)()
        return list(buf[:max(0, ctypes.CDLL("/usr/lib/libSystem.B.dylib").proc_listchildpids(pid, buf, 16384))])
    with contextlib.suppress(OSError, ValueError), open(f"/proc/{pid}/task/{pid}/children") as f:
        return [int(n) for n in f.read().split()]
    return []


def _kill():
    groups, tree = set(_groups), [os.getpid()]
    for parent in tree:
        for pid in _children(parent):
            tree.append(pid)
            with contextlib.suppress(OSError):
                groups.add(os.getpgid(pid))
    for group in groups - {0, 1, os.getpgrp()}:
        with contextlib.suppress(OSError):
            os.killpg(group, signal.SIGKILL)
    # low starts the worker as the leader of its own group; started any other way, the group is whoever started it.
    if os.getpgrp() == os.getpid():
        os.killpg(0, signal.SIGKILL)
    os.kill(os.getpid(), signal.SIGKILL)


def _reap():
    """Stops what a run left running, a server or watcher started with subprocess.Popen say: it holds the run's
    capture, a deleted file nothing reads or caps once the run is over, and would fill the disk from there. Returns
    how many processes it stopped."""
    tree = _children(os.getpid())
    for parent in tree:
        tree.extend(_children(parent))
    groups = set()
    for pid in tree:
        with contextlib.suppress(OSError):
            groups.add(os.getpgid(pid))
    for group in groups - {0, 1, os.getpgrp()}:
        with contextlib.suppress(OSError):
            os.killpg(group, signal.SIGKILL)
    for pid in tree:
        with contextlib.suppress(OSError):
            os.kill(pid, signal.SIGKILL)
    for pid in _children(os.getpid()):
        with contextlib.suppress(OSError):
            os.waitpid(pid, 0)
    return len(tree)


def _watch(parent):
    global _dropped
    while os.getppid() == parent:
        time.sleep(0.25)
        # Code that prints without end would fill the disk with its capture: past OUT_CAP its output goes nowhere.
        with _flow:
            if _running and not _dropped and os.fstat(1).st_size > OUT_CAP:
                os.dup2(_null, 1)
                os.dup2(_null, 2)
                _dropped = True
    _kill()


threading.Thread(target=_watch, args=(os.getppid(),), daemon=True).start()


def _exec(cmd, timeout):
    left = _deadline - time.monotonic()
    if not timeout or timeout > left:
        timeout = max(1, int(left))
    p = _Popen(cmd, shell=True, cwd=ROOT, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
               stderr=subprocess.STDOUT, start_new_session=True)
    _groups.add(p.pid)
    # Read as it comes, keeping both ends in memory, so a command that prints without end fills no disk.
    head, tail, size = bytearray(), bytearray(), [0]

    def drain():
        for chunk in iter(lambda: p.stdout.read1(1 << 16), b""):
            size[0] += len(chunk)
            take = max(0, LIMIT // 2 - len(head))
            head.extend(chunk[:take])
            tail.extend(chunk[take:])
            if len(tail) > LIMIT:
                del tail[:len(tail) - LIMIT // 2]
    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    try:
        code = p.wait(timeout=timeout)
    except BaseException as e:
        with contextlib.suppress(OSError):
            os.killpg(p.pid, signal.SIGKILL)
        p.wait()
        if not isinstance(e, subprocess.TimeoutExpired):
            raise
        code = f"timeout after {timeout}s"
    with contextlib.suppress(OSError):
        os.killpg(p.pid, signal.SIGKILL)
    # A process that left the group can hold the pipe open: what it prints after a second is not waited for.
    reader.join(1)
    _groups.discard(p.pid)
    start, end = bytes(head), bytes(tail[-(LIMIT // 2):])
    gap = size[0] - len(start) - len(end)
    out = start + (f"\n[… {gap} bytes omitted]\n".encode() if gap > 0 else b"") + end
    return ANSI.sub("", out.decode("utf-8", "replace")), code


def _read(f):
    size = f.seek(0, 2)
    f.seek(0)
    if size <= LIMIT:
        return f.read()
    head = f.read(LIMIT // 2)
    f.seek(-LIMIT // 2, 2)
    return head + f"\n[… {size - LIMIT} bytes omitted]\n".encode() + f.read()


def _spill(text, n):
    """Keeps a run's whole output, of which the model gets only the ends, where show() and sh() can read it: in low's
    cache for this repo, outside the tree, with the newest SPILLS kept. Its path, or None when it can't be written."""
    folder = os.path.join(os.environ.get("XDG_CACHE_HOME") or tempfile.gettempdir(), "low-out")
    path = os.path.join(folder, f"run-{os.getpid()}-{n}.txt")
    try:
        os.makedirs(folder, exist_ok=True)
        with open(path, "w", encoding="utf-8", errors="replace") as f:
            f.write(text)
    except OSError:
        return None
    with contextlib.suppress(OSError):
        kept = [e for e in os.scandir(folder) if e.name.startswith("run-")]
        kept.sort(key=lambda e: e.stat().st_mtime_ns)
        for e in kept[:-SPILLS]:
            with contextlib.suppress(OSError):
                os.unlink(e.path)
    return path


def _rel(path):
    return os.path.relpath(os.path.join(ROOT, path), ROOT)


def _shown(path):
    """A path as show() names it: from the repo root, or whole when it is outside the repo, as a run's output file is."""
    rel = _rel(path)
    return os.path.abspath(os.path.join(ROOT, path)) if rel.startswith("..") else rel


def _say(text):
    """Prints a helper's own line and counts it, so finish() can tell what else the run printed."""
    global _said
    sys.stdout.flush()
    before = os.fstat(1).st_size
    print(text)
    sys.stdout.flush()
    _said += os.fstat(1).st_size - before


def _guide(path):
    """Prints the AGENTS.md (else CLAUDE.md) of each folder between the repo root and path, outermost first, unless the
    model's context already holds it: the root's is in the system prompt, a folder's own the first time a run shows,
    edits or writes a file under it. low passes the ones still in context as state["guides"]."""
    real = os.path.realpath(os.path.join(ROOT, path))
    if READONLY or not real.startswith(ROOT + os.sep):
        return
    seen = set(_state.get("guides") or ()) | set(_report.get("guides", ()))
    parts = os.path.relpath(os.path.dirname(real), ROOT).split(os.sep)
    for k in range(1, len(parts) + 1 if parts != ["."] else 1):
        folder = os.path.join(*parts[:k])
        found = (os.path.join(folder, name) for name in GUIDES)
        rel = next((r for r in found if os.path.isfile(os.path.join(ROOT, r))), None)
        full = rel and os.path.realpath(os.path.join(ROOT, rel))
        if not rel or rel in seen or not full.startswith(ROOT + os.sep):
            continue
        _report.setdefault("guides", []).append(rel)
        if full == real:
            continue  # show() is about to print it
        try:
            with open(full, encoding="utf-8", errors="replace") as f:
                text = f.read(GUIDE_CAP + 1)
        except OSError:
            continue
        more = ""
        if len(text) > GUIDE_CAP:
            text = text[:text.rfind("\n", 0, GUIDE_CAP) + 1] or text[:GUIDE_CAP]
            more = f"; show({rel!r}, {text.count(chr(10)) + 1}) has the rest"
        print(f"[{rel}: follow it for the files in {folder}/; shown once{more}]\n{text.rstrip()}\n[end of {rel}]")


def _change(tool, path, make, fmt=True):
    rel = _rel(path)
    real = os.path.realpath(os.path.join(ROOT, rel))
    signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT})
    try:
        if READONLY:
            raise ValueError(f"{path} stays as it is: this worker only reads, to check the report's claims")
        if rel.startswith("..") or not real.startswith(ROOT + os.sep):
            raise ValueError(f"{path} is outside the repo")
        if IGNORABLE.search(rel):
            raise ValueError(f"{path!r} has invisible or bidi characters in its name; low will not change it")
        parts = (rel + os.sep + os.path.relpath(real, ROOT)).split(os.sep)
        if any(d == real or real.startswith(d + os.sep) for d in LOCKED):
            raise ValueError(f"{path} is protected (protect in low.toml, or --protect); low will not change it")
        if any(d == real or real.startswith(d + os.sep) for d in PROTECTED) or any(
                IGNORABLE.sub("", p).casefold() == ".git" for p in parts):
            raise ValueError(f"{path} is off limits: git runs code from there outside the sandbox")
        st = os.stat(real) if os.path.lexists(real) else None
        if st and stat.S_ISREG(st.st_mode) and (st.st_nlink > 1 or st.st_dev != os.stat(ROOT).st_dev):
            raise ValueError(f"{path} has other hard links or lives on another volume; low will not change it")
        before, written, *notes = make(os.path.join(ROOT, rel), rel)
        notes = notes[0] if notes else []
        delta = [l[0] for l in difflib.unified_diff(before.splitlines(), written.splitlines(), lineterm="")][2:]
        action = {"tool": tool, "path": rel, "ok": True, "added": delta.count("+"), "removed": delta.count("-")}
        if tool == "write" and st:
            action["replaced"] = True
        if notes:
            action["reindented"] = len(notes)
        _report.setdefault("actions", []).append(action)
        _turn.setdefault("touched", set()).add(rel)
        if tool == "write" and not st:
            _turn.setdefault("created", set()).add(rel)
    except Exception as e:
        _report.setdefault("actions", []).append({"tool": tool, "path": rel, "ok": False, "error": str(e)})
        raise
    finally:
        signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGINT})
    _forget()
    if not os.path.lexists(os.path.join(ROOT, rel)):
        _say(f"{tool} {rel}: removed")
        return
    note = "".join(f"; {n}" for n in notes)
    if FMT and fmt:
        out, code = _exec(f"{FMT} {shlex.quote('./' + rel if rel.startswith('-') else rel)}", 60)
        note = f"; fmt failed: {out.strip().splitlines()[0] if out.strip() else code}" if code else ""
    with open(os.path.join(ROOT, rel), encoding="utf-8", errors="replace") as f:
        after = f.read()
    written = written.replace("\r\n", "\n")
    if after != written:
        diff = difflib.unified_diff(written.splitlines(True), after.splitlines(True), n=1)
        note += "; the formatter changed it:\n" + "".join(list(diff)[2:]).rstrip("\n")
    _say(f"{tool} {rel}: ok{note}")
    _guide(rel)


def _forget():
    """Unloads the repo's modules after a change. Names that runs bound to one of them, or to a function or class from
    one, are rebound to the new code; anything else from them (instances) is dropped, as is a name whose module no
    longer imports."""
    stale = {}
    for name, mod in list(sys.modules.items()):
        path = getattr(mod, "__file__", None)
        if path and "site-packages" not in path and os.path.realpath(path).startswith(ROOT + os.sep):
            stale[name] = sys.modules.pop(name)
    fresh = {}

    def load(name):
        if name not in fresh:
            fresh[name] = importlib.import_module(name)
            for sub in [s for s in stale if s.startswith(name + ".")]:
                with contextlib.suppress(Exception):
                    importlib.import_module(sub)
        return fresh[name]
    for key, value in list(G.items()):
        module, owner = next((n for n, m in stale.items() if m is value), None), None
        with contextlib.suppress(Exception):
            owner = module or getattr(value, "__module__", None)
        if owner not in stale:
            continue
        attr = getattr(value, "__name__", None) if module is None else None
        try:
            if module:
                G[key] = load(module)
            elif attr and getattr(stale[owner], attr, None) is value:
                G[key] = getattr(load(owner), attr)
            else:
                del G[key]
        except Exception:
            G.pop(key, None)


def _differ(line, want, n):
    """Where file line n and a line of old first differ, both without their indentation and trailing whitespace."""
    text = line.strip()
    i = next((i for i, (a, b) in enumerate(zip(text, want)) if a != b), min(len(text), len(want)))

    def piece(s):
        if not s:
            return "a blank line"
        return "the end of the line" if i >= len(s) else repr(s[i:i + 30]) + ("…" if len(s) > i + 30 else "")
    column = len(line) - len(line.lstrip()) + i + 1
    return f"line {n} differs at column {column}: the file has {piece(text)}, old has {piece(want)}"


def _around(lines, first, last):
    """A match of old, lines first..last, with two lines either side, numbered as show() numbers them. A longer match
    keeps only its ends: every match has the same lines in between."""
    total = len(lines) - (lines[-1] == "")  # show() counts no line after a final newline
    keep = [*range(max(first - 2, 1), first + 1), *([last] if last > first else []),
            *range(last + 1, min(last + 2, total) + 1)]
    rows = []
    for k, n in enumerate(keep):
        if k and n > keep[k - 1] + 1:
            rows.append("…")
        rows.append(f"{n}: {lines[n - 1][:200]}{'…' if len(lines[n - 1]) > 200 else ''}")
    return rows


def _near(text, old, rel):
    """Where a failed edit's old text is: each match with the lines around it, or the lines most like it and where they
    first differ, so the next run can fix old without first showing the file again."""
    if not old.strip():
        return None
    count, lead = text.count(old), len(old) - len(old.lstrip("\n"))
    if count > 1:
        at, i = [], text.find(old)
        while i != -1 and len(at) < 10:
            at.append(text.count("\n", 0, i + lead) + 1)
            i = text.find(old, i + len(old))
        more = ", …" if count > 10 else ""
        lines, span = text.split("\n"), old.strip("\n").count("\n")
        shown = "\n--\n".join("\n".join(_around(lines, n, n + span)) for n in at[:3])
        return f"[{rel}: old occurs at lines {', '.join(map(str, at))}{more}; add context so it matches once]\n{shown}"
    # Models write \" in raw strings, where the backslash stays, \n where they mean a line break, and \\ for one
    # backslash.
    bare = re.sub(r"\\([\\\"'nt])", lambda m: {"n": "\n", "t": "\t"}.get(m[1], m[1]), old)
    if bare != old and text.count(bare) == 1:
        return (f"[{rel}: old matches once with its backslash escapes read as a normal string reads them; a raw string "
                "keeps them as typed]")
    lines, want = text.split("\n"), [w.strip() for w in old.strip("\n").split("\n")]
    trimmed = [line.strip() for line in lines]
    # Each file line equal to a line of old votes for where old would start; short lines such as } say too little.
    where, votes = {}, {}
    for i, w in enumerate(want):
        if len(w) > 3:
            where.setdefault(w, []).append(i)
    for j, line in enumerate(trimmed):
        for i in where.get(line, ()):
            votes[j - i] = votes.get(j - i, 0) + 1
    if votes:
        start = max(votes, key=lambda s: (votes[s], -s))
        alike = f"{votes[start]} of {len(want)} lines alike"
    else:
        # No line is the same: align on the file line closest to old's longest one.
        i = max(range(len(want)), key=lambda i: len(want[i]))
        close = difflib.get_close_matches(want[i], trimmed[:20000], n=1, cutoff=0.6)
        if not close:
            return f"[{rel}: no line of old is in the file; show() it before editing]"
        start, alike = trimmed.index(close[0]) - i, "the closest match"
    start = max(start, 0)
    end = min(start + len(want), len(lines))
    first = next((j for j in range(start, end) if trimmed[j] != want[j - start]), None)
    if [line.split() for line in lines[start:end]] == [w.split() for w in want]:
        alike = "it differs only in whitespace"
    elif first is not None:
        alike += "; " + _differ(lines[first], want[first - start], first + 1)
    shown = enumerate(lines[start:end][:12], start + 1)
    rows = [f"{n}: {line[:200]}{'…' if len(line) > 200 else ''}" for n, line in shown]
    more = f"; show({rel!r}, {start + 13}, {end}) has the rest" if end - start > 12 else ""
    span = f"line {end}" if end - start == 1 else f"lines {start + 1}-{end}"
    return f"[{rel}: nearest to old is {span}, {alike}{more}]\n" + "\n".join(rows)


def _indent(line):
    return line[:len(line) - len(line.lstrip())]


def _shifted(text, old, new):
    """old matched by whole lines once with every line indented deeper or shallower by the same amount: text with them
    replaced by new indented alike, and what was done; None when that is not the case."""
    core = old.strip("\n")
    lead, trail = len(old) - len(old.lstrip("\n")), len(old) - len(old.rstrip("\n"))
    if not core.strip() or new[:lead] != "\n" * lead or trail and new[len(new) - trail:] != "\n" * trail:
        return None
    new = new[lead:len(new) - trail]
    lines, want = text.split("\n"), core.split("\n")
    stripped = [w.strip() for w in want]
    first = next(k for k, w in enumerate(stripped) if w)
    found = [i - first for i, line in enumerate(lines) if line.strip() == stripped[first] and i >= first]
    found = [i for i in found if [line.strip() for line in lines[i:i + len(want)]] == stripped]
    if len(found) != 1:
        return None
    start = found[0]
    pairs = [(_indent(f), _indent(w)) for f, w in zip(lines[start:start + len(want)], want) if w.strip()]
    deeper = pairs[0][0].endswith(pairs[0][1]) and len(pairs[0][0]) > len(pairs[0][1])
    shift = (pairs[0][0][:len(pairs[0][0]) - len(pairs[0][1])] if deeper
             else pairs[0][1][:len(pairs[0][1]) - len(pairs[0][0])])
    if not shift or any(f != (shift + w if deeper else w[len(shift):]) or not deeper and not w.startswith(shift)
                        for f, w in pairs):
        return None
    out = []
    for line in new.split("\n"):
        if not line.strip():
            out.append(line)
        elif deeper:
            out.append(shift + line)
        elif line.startswith(shift):
            out.append(line[len(shift):])
        else:
            return None
    end = start + len(want)
    kind = "tab" if shift == "\t" * len(shift) else "space" if shift == " " * len(shift) else "blank"
    width = f"{len(shift)} {kind}{'s' if len(shift) > 1 else ''}"
    span = f"line {start + 1}" if end - start == 1 else f"lines {start + 1}-{end}"
    verb, way = "matches" if end - start == 1 else "match", "deeper" if deeper else "shallower"
    note = f"{span} {verb} old indented {width} {way}, so new was too"
    return "\n".join(lines[:start] + out + lines[end:]), note


def edit(path, old, new=None):
    pair = lambda p: isinstance(p, (list, tuple)) and len(p) == 2 and all(isinstance(s, str) for s in p)  # noqa: E731
    texts = isinstance(old, (list, tuple)) and all(isinstance(s, str) for s in old)
    if isinstance(old, str) and isinstance(new, str):
        pairs = [(old, new)]
    elif new is None and isinstance(old, (list, tuple)) and old and all(pair(p) for p in old):
        pairs = [tuple(p) for p in old]
    elif new is None and texts and old and len(old) % 2 == 0:
        # [old, new] and [old1, new1, old2, new2, ...]: not the forms the prompt names, but models keep writing them.
        pairs = list(zip(old[::2], old[1::2]))
    else:
        raise TypeError("edit() takes (path, old, new) or (path, [(old, new), ...]) with string old and new")
    def make(full, rel):
        with open(full, encoding="utf-8", newline="") as f:
            raw = f.read()
        crlf = "\r\n" in raw and raw.count("\r\n") == raw.count("\n")
        eol = (lambda s: s.replace("\r\n", "\n")) if crlf else (lambda s: s)
        text = result = eol(raw)
        notes = []
        for k, (o, n) in enumerate(pairs, 1):
            find = eol(o)
            count = result.count(find)
            which = f"pair {k} of {len(pairs)}" if len(pairs) > 1 else ""
            # An old whose lines are all indented too deep or too shallow by the same amount still means one place.
            if count == 0 and (shifted := _shifted(result, find, eol(n))):
                result = shifted[0]
                notes.append(f"{which}: {shifted[1]}" if which else shifted[1])
                continue
            if count != 1:
                if hint := _near(result, find, rel):
                    _say(hint)
                which = f" ({which}; nothing was changed)" if which else ""
                raise ValueError(f"old text {'not found' if count == 0 else f'occurs {count} times'} in {rel}{which}")
            result = result.replace(find, eol(n), 1)
        data = (result.replace("\n", "\r\n") if crlf else result).encode("utf-8")
        with open(full, "wb") as f:
            f.write(data)
        return text, result, notes
    _change("edit", path, make)


def write(path, content):
    def make(full, rel):
        before, text = "", content
        if os.path.lexists(full):
            if os.path.islink(full) or not os.path.isfile(full):
                raise ValueError(f"{rel} exists and is not a regular file")
            with open(full, encoding="utf-8", newline="") as f:
                before = f.read()
            # A file with CRLF line ends keeps them, as it does under edit().
            if "\r\n" in before and before.count("\r\n") == before.count("\n") and "\r" not in content:
                text = content.replace("\n", "\r\n")
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "wb") as f:
            f.write(text.encode("utf-8"))
        return before, text
    _change("write", path, make)


def undo(*paths):
    tree = str(_state.get("snapshot") or "").split(" ")[0]
    if not tree:
        raise ValueError("undo() needs the snapshot low takes when a turn begins, and it takes none outside a git repository")
    paths = paths or sorted(_turn.get("touched", ()))
    if not paths:
        _say("[undo: edit() and write() changed nothing this turn]")
    for path in paths:
        rel = _rel(path)
        p = _Popen(["git", "cat-file", "blob", f"{tree}:{rel}"], cwd=ROOT, stdout=subprocess.PIPE,
                   stderr=subprocess.DEVNULL)
        blob = p.communicate()[0]
        if p.returncode and rel not in _turn.get("created", ()):
            raise ValueError(f"{rel} was not in the snapshot taken when this turn began, and write() did not create it")
        if p.returncode and not os.path.lexists(os.path.join(ROOT, rel)):
            continue  # created by write() and removed already
        _change("undo", rel, lambda full, rel: _put(full, None if p.returncode else blob), fmt=False)


def _put(full, data):
    """Writes data (bytes) to full, or removes it when data is None; returns the text before and after."""
    before = ""
    if os.path.lexists(full):
        with open(full, encoding="utf-8", errors="replace") as f:
            before = f.read()
    if data is None:
        os.remove(full)
        return before, ""
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "wb") as f:
        f.write(data)
    return before, data.decode("utf-8", "replace")


def _commands(cmd):
    """The commands a shell line runs, each as its name and arguments; none when it does not parse."""
    lexer = shlex.shlex(str(cmd), posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    try:
        words = list(lexer)
    except ValueError:
        return
    first = True  # where a command starts: the beginning, after an operator, after VAR=value
    for i, word in enumerate(words):
        if word.strip("();<>|&") == "":
            first = True
            continue
        if first and "=" in word and not word.startswith("-"):
            continue
        if first:
            rest = []
            for w in words[i + 1:]:
                if w.strip("();<>|&") == "":
                    break
                rest.append(w)
            yield os.path.basename(word), rest
        first = False


# What rm -r is given to empty the whole tree: the root itself, or every top-level name or dotfile.
_EVERYTHING = {"*", ".*", ".[!.]*", ".[^.]*", "..?*", ".??*"}
# What an operand can start with that the shell, not rm, turns into the working directory or the home.
_HERE = re.compile(r"^(\$PWD|\$\{PWD\}|~)(?=/|$)")


def _cleans_ignored(cmd):
    # git clean -x or -X, not a dry run, or rm -r of the whole tree: either deletes the files git ignores, which no
    # undo restores.
    for name, rest in _commands(cmd):
        if name == "git" and "clean" in rest:
            flags = [w for w in rest[rest.index("clean") + 1:] if w.startswith("-") and not w.startswith("--")]
            if any(set(f) & {"x", "X"} for f in flags) and not any("n" in f for f in flags) and "--dry-run" not in rest:
                return "git clean -x or -X"
        if name == "rm" and any(w == "--recursive" or w[:1] == "-" and w[1:2] != "-" and set(w) & {"r", "R"}
                                for w in rest):
            for w in (w for w in rest if not w.startswith("-")):
                here = _HERE.sub(lambda m: os.path.expanduser("~") if m[0] == "~" else ROOT, w)
                path = os.path.normpath(os.path.join(ROOT, here))
                if path == ROOT or os.path.dirname(path) == ROOT and os.path.basename(path) in _EVERYTHING:
                    return f"rm -r {w}"
    return None


def _listed(cmd):
    """Lists a command the run starts, as the turn's report, a second look and check()'s freshness count them; with
    the refusal when it would delete the files git ignores."""
    entry = [str(cmd), None, []]
    _report.setdefault("cmds", []).append(entry)
    what = _cleans_ignored(cmd)
    if not what:
        return entry, None
    entry[1] = "refused"
    listed = _Popen(["git", "ls-files", "-o", "-i", "--exclude-standard", "--directory"], cwd=ROOT, text=True,
                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL).communicate()[0].split()
    some = ", ".join(listed[:5]) + (f" and {len(listed) - 5} more" if len(listed) > 5 else "")
    return entry, (f"[refused: {what} deletes the files git ignores ({some or 'none now'}), and no undo restores them; "
                   "delete the ones you mean by name]")


def _files(cmd):
    return [_rel(w) for w in str(cmd).split() if os.path.isfile(os.path.join(ROOT, w))]


def sh(cmd, timeout=120):
    entry, refused = _listed(cmd)
    if refused:
        return refused
    out, code = _exec(cmd, timeout)
    entry[1:] = [code, _files(cmd)]
    return out.rstrip("\n") + (f"\n[exit {code}]" if code else "")


class _Seen(_Popen):
    """subprocess.Popen as the run's code gets it, through run(), check_output() and os.popen() too: its command is
    listed and refused as sh()'s is, and its exit status is filled in when the run ends. In the session logs about one
    run in 20 started its commands this way, mostly a grep, which low saw nothing of."""

    def __init__(self, args, *rest, **kwargs):
        try:
            if isinstance(args, (str, bytes, os.PathLike)):
                cmd = os.fsdecode(args)
            else:
                args = list(args)
                cmd = shlex.join(map(os.fsdecode, args))
        except TypeError:  # what Popen itself turns down, as it then does
            cmd = str(args)
        entry, refused = _listed(cmd)
        if refused:
            raise PermissionError(refused)
        entry[2] = _files(cmd)
        try:
            super().__init__(args, *rest, **kwargs)
        except BaseException as e:
            entry[1] = type(e).__name__
            raise
        _spawned.append((entry, self))


_Seen.__name__ = _Seen.__qualname__ = "Popen"  # as a printed handle or a traceback names it


def _system(command):
    """os.system as the run's code gets it, listed and refused as sh() is."""
    entry, refused = _listed(os.fsdecode(command))
    if refused:
        raise PermissionError(refused)
    entry[2] = _files(entry[0])
    status = _os_system(command)
    try:
        entry[1] = os.waitstatus_to_exitcode(status)
    except ValueError:
        entry[1] = status
    return status


def _block(lines, i):
    """Lines (0-based, inclusive) of the definition around line i: the nearest def/fn/class/... line at or above it and
    less indented, else the top-level statement it is in, down to the line before the next one at that indentation (a
    closing bracket line included). Past 200 lines, the 10 lines above i and the 40 below instead."""
    width = lambda s: len(s) - len(s.lstrip())  # noqa: E731
    k = width(lines[i])
    start = next((j for j in range(i, -1, -1)
                  if lines[j].strip() and DEFINITION.match(lines[j]) and (j == i or width(lines[j]) < k)), None)
    if start is None:
        start = next((j for j in range(i, -1, -1) if lines[j].strip() and not width(lines[j])), i)
    end, k = len(lines) - 1, width(lines[start])
    for j in range(start + 1, len(lines)):
        text = lines[j].strip()
        if text and width(lines[j]) <= k and not (text[0] in ")]}" and text.endswith(("{", ":", "(", "[", "=>"))):
            end = j if text[0] in ")]}" or re.match(r"end\b", text) else j - 1
            break
    while end > start and not lines[end].strip():
        end -= 1
    if start == end:  # no structure around it, as in prose: its paragraph
        while start > 0 and lines[start - 1].strip():
            start -= 1
        while end + 1 < len(lines) and lines[end + 1].strip():
            end += 1
    while start > 0 and lines[start - 1].lstrip().startswith(("@", "#[", "///", "//!")):
        start -= 1
    return (start, end) if end - start < 200 else (max(0, i - 10), min(len(lines) - 1, i + 40))


def _blocks(path, full, pattern):
    """show(path, regex): each definition with a line matching pattern, numbered, a gap between them marked."""
    rel = _shown(path)
    with open(full, encoding="utf-8", errors="replace") as f:
        lines = f.read().splitlines()
    spans = []
    for a, b in sorted(_block(lines, i) for i, line in enumerate(lines) if re.search(pattern, line)):
        if spans and a <= spans[-1][1] + 1:
            spans[-1][1] = max(spans[-1][1], b)
        else:
            spans.append([a, b])
    if not spans:
        print(f"[{rel}: no line matches {pattern!r}]")
        return
    numbers = [n for a, b in spans for n in range(a + 1, b + 2)]
    sys.stdout.flush()
    room, rows, shown = 2 * HALF - os.fstat(1).st_size - 2000, [], 0
    for n in numbers:
        room -= len(lines[n - 1]) + len(str(n)) + 3
        if room < 0 and shown:
            break
        if shown and n != numbers[shown - 1] + 1:
            rows.append("…")
        rows.append(f"{n}: {lines[n - 1]}")
        shown += 1
    where = ", ".join(f"{a + 1}-{b + 1}" if b > a else f"{a + 1}" for a, b in spans)
    rest = f"; output limit: show({rel!r}, {numbers[shown]}) continues" if shown < len(numbers) else ""
    print(f"[{rel}: the blocks with a line matching {pattern!r}, lines {where} of {len(lines)}{rest}]")
    print("\n".join(rows))


def show(path, start=1, end=None):
    full = os.path.join(ROOT, path)
    with open(full, "rb") as f:
        _report.setdefault("reads", []).append(_shown(path))
        _guide(path)
        if b"\0" in f.read(8192):
            print(f"[{_shown(path)}: binary, {os.fstat(f.fileno()).st_size} bytes]")
            return
    if isinstance(start, str):
        return _blocks(path, full, start)
    asked, start = end, max(start, 1)
    sys.stdout.flush()
    room, rows, total = 2 * HALF - os.fstat(1).st_size - 2000, [], 0
    # Line by line, so a huge file is counted, not held.
    with open(full, encoding="utf-8", errors="replace") as f:
        for total, line in enumerate(f, 1):
            if start <= total and (not asked or total <= asked) and room >= 0:
                line = line[:-1] if line.endswith("\n") else line
                room -= len(line) + len(str(total)) + 3
                if room >= 0 or not rows:
                    rows.append(f"{total}: {line}")
    if start > total:
        print(f"[{_shown(path)}: empty]" if not total else f"[{_shown(path)}: {total} lines; nothing at line {start}]")
        return
    end, stop = min(asked or total, total), start + len(rows) - 1
    rest = f"; show({_shown(path)!r}, {stop + 1}{f', {asked}' if asked else ''}) continues" if stop < end else ""
    print(f"[{_shown(path)}: lines {start}-{stop} of {total}{rest}]")
    print("\n".join(rows))


def recall(n, start=1, end=None):
    """Prints entry #n of the session, one a summary replaced, as it was. The sandbox hides the log, so low reads it;
    show() pages it from a temporary file, under the #n it goes by, and each call asks for it again."""
    n = int(str(n).lstrip("#"))
    answer = _host({"needs_check": True, "recall": n})
    if answer.get("ok") is not True:
        print(f"[#{n}: {answer.get('out') or 'low could not read it'}]")
        return
    fd, path = tempfile.mkstemp(prefix=f"low-entry-{n}-", suffix=".txt")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", errors="replace") as f:
            f.write((answer.get("out") or "").replace("\0", "\\0"))  # show() takes a NUL for a binary file
        # show() sizes the page by what the run printed so far, which has to be on fd 1 before stdout is redirected.
        sys.stdout.flush()
        page = io.StringIO()
        with contextlib.redirect_stdout(page):
            show(path, start, end)
    finally:
        with contextlib.suppress(OSError):
            os.unlink(path)
    _report["reads"][-1] = f"#{n}"
    text = page.getvalue()
    # show() heads the page with the name it gives the file, which goes by #n here.
    name = text[1 : text.find(": ")]
    print(text.replace(f"show({name!r}, ", f"recall({n}, ").replace(f"[{name}:", f"[#{n}:", 1), end="")


def accept(criteria):
    _report["accepted"] = str(criteria)


def check():
    # Nothing configured, nothing to fail: `assert check()`, which ends most verifying runs, used to raise here.
    if not CHECK:
        _say("[no check configured: check() ran nothing]")
        return True
    ran = len(_report.get("cmds", []))
    answer = _host({"needs_check": True, "actions": _report.get("actions", []), "cmds": _report.get("cmds", [])})
    ok = answer.get("ok") is True
    global _checked
    _checked = _tree() if ok else None
    _report["check"] = ok
    _report.setdefault("actions", []).append({"tool": "check", "ok": ok, "cmds": ran})
    _say(answer.get("out") or f"[check {'passed' if ok else 'FAILED'}]")
    return ok


def _tree():
    out = _Popen(["git", "ls-files", "-co", "--exclude-standard", "-z"], cwd=ROOT, stdout=subprocess.PIPE,
                 stderr=subprocess.DEVNULL).communicate()[0]
    tree = {}
    for f in filter(None, out.decode("utf-8", "replace").split("\0")):
        with contextlib.suppress(OSError):
            st = os.stat(os.path.join(ROOT, f))
            tree[f] = (st.st_size, st.st_mtime_ns)
    return tree


def plan(steps):
    _report["plan"] = {"steps": [str(s) for s in steps], "done": []}


def done(step=None):
    p = _report.get("plan") or _state.get("plan") or {"steps": [], "done": []}
    left = [i for i in range(1, len(p["steps"]) + 1) if i not in p["done"]]
    n = step if isinstance(step, int) else (left[0] if left else None)
    if n is None:
        return None
    if not 1 <= n <= len(p["steps"]):
        raise ValueError(f"no step {n}; the plan has {len(p['steps'])}")
    _report["plan"] = {"steps": p["steps"], "done": sorted(set(p["done"]) | {n})}
    rest = [i for i in left if i != n]
    return p["steps"][rest[0] - 1] if rest else None


def finish(report):
    _report["finish"] = str(report)


def ask(question):
    _report["ask"] = str(question)


def _web(requests):
    # The sandbox has no network: the host makes the requests, several at once, and sends back each one's [ok, response
    # or why it failed], in order.
    _report.setdefault("web", []).extend(requests)
    answer = _host({"needs_check": True, "web": requests})
    if answer.get("ok") is not True:
        raise RuntimeError(answer.get("out") or "the web request failed")
    return json.loads(answer["out"])


def _each(arg, request, shape):
    # One request, or a list of up to 50 made at once: a list gets each answer in its request's place, the RuntimeError
    # of one that failed included, where one request raises it.
    many = isinstance(arg, (list, tuple))
    items = list(arg) if many else [arg]
    if len(items) > 50:
        raise ValueError(f"{len(items)} requests in one list; up to 50 go at once, so split it")
    answers = []
    for ok, out in _web([request(item) for item in items]) if items else []:
        try:
            answers.append(shape(json.loads(out)) if ok else RuntimeError(out or "the web request failed"))
        except (AttributeError, TypeError, ValueError) as e:
            answers.append(RuntimeError(f"the tools API sent an answer low can't read: {e}"))
    if many:
        return answers
    if isinstance(answers[0], Exception):
        raise answers[0]
    return answers[0]


def search(query, count=10):
    count = min(max(int(count), 1), 50)
    keep = ("title", "url", "summary", "date", "site_name")
    return _each(query, lambda q: {"search": str(q), "count": count},
                 lambda found: [{k: r.get(k) for k in keep} for r in found.get("results") or []])


def fetch(url):
    # DeepSeek escapes every punctuation mark (\{\"name\"\: ...): undone, a JSON API's body parses and code reads as
    # written. Only when every backslash is such an escape, so text that came back unescaped stays as it is. What low
    # fetched itself (raw) is as the host served it: its \" in a JSON string is no escape of DeepSeek's.
    def text(found):
        text = found.get("content") or ""
        return text if found.get("raw") or "\\" in ESCAPED.sub("", text) else ESCAPED.sub(r"\1", text)
    return _each(url, lambda u: {"fetch": str(u)}, text)


TOOLS = {"sh": sh, "show": show, "edit": edit, "write": write, "undo": undo, "accept": accept, "plan": plan, "done": done,
         "check": check, "finish": finish, "ask": ask, "search": search, "fetch": fetch, "recall": recall}
G = {"__name__": "__main__", **TOOLS}
HELPERS = tuple(f"{name}()" for name in TOOLS)


class _Helpers:
    def find_spec(self, name, path=None, target=None):
        if path is None and re.fullmatch(rf"({'|'.join(TOOLS)})(_\w*)?", name):
            return importlib.util.spec_from_loader(name, self)
        return None

    def create_module(self, spec):
        return TOOLS[spec.name.split("_")[0]]

    def exec_module(self, module):
        print(f"[import {module.__spec__.name}: low's helpers are built in; no import needed]")


sh.sh = sys.modules["sh"] = sh
subprocess.Popen, os.system = _Seen, _system
sys.meta_path.append(_Helpers())
signal.signal(signal.SIGINT, signal.SIG_IGN)
signal.signal(signal.SIGALRM, _overrun)
_outbox.write(json.dumps({"ready": True, "python": sys.version.split()[0]}) + "\n")
_outbox.flush()
for n, request in enumerate(_inbox, 1):
    request = json.loads(request)
    code, _state = request["code"], request.get("state") or {}
    if _state.get("turn") != _turn.get("turn"):
        _turn = {"turn": _state.get("turn"), "touched": set(_state.get("touched") or ()),
                 "created": set(_state.get("created") or ())}
    _deadline = time.monotonic() + RUN_CAP
    _report.clear()
    _said = 0
    os.chdir(ROOT)
    G.update(TOOLS)
    capture = tempfile.TemporaryFile()
    with _flow:
        os.dup2(capture.fileno(), 1)
        os.dup2(capture.fileno(), 2)
        _running, _dropped = True, False
    sys.stdout = sys.stderr = open(1, "w", encoding="utf-8", errors="replace", buffering=1, closefd=False)
    failed = False
    signal.signal(signal.SIGINT, signal.default_int_handler)
    try:
        # Each run's code under its own name, so a traceback through a function an earlier run defined shows its line.
        name = f"<run {n}>"
        linecache.cache[name] = (len(code), None, code.splitlines(True), name)
        tree = ast.parse(code, name)
        tail = tree.body.pop() if tree.body and isinstance(tree.body[-1], ast.Expr) else None
        # The alarm only while the model's code runs: past it, an exception would land in the worker's own.
        signal.signal(signal.SIGALRM, _overrun)
        _executing = True
        signal.setitimer(signal.ITIMER_REAL, RUN_CAP + 5)
        try:
            exec(compile(tree, name, "exec"), G)
            value = eval(compile(ast.Expression(tail.value), name, "eval"), G) if tail else None
        finally:
            _executing = False
            signal.setitimer(signal.ITIMER_REAL, 0)
        sys.stdout.flush()
        if value is not None and not os.fstat(1).st_size:
            print(value)
    except BaseException as e:
        failed = True
        tb = traceback.extract_tb(e.__traceback__)
        # The runs' own frames and the repo's files, not the worker's helpers or installed packages.
        ours = lambda f: f.startswith(ROOT + os.sep) and "site-packages" not in f  # noqa: E731
        frames = [f for f in tb if f.filename.startswith("<run ") or ours(f.filename)]
        for f in frames:
            if not f.filename.startswith("<run "):
                f.filename = os.path.relpath(f.filename, ROOT)
        misuse = isinstance(e, TypeError) and tb and tb[-1].filename.startswith("<run ") and str(e).startswith(HELPERS)
        rejected = isinstance(e, (SyntaxError, NameError)) or misuse
        _report["exception"], _report["rejected"] = type(e).__name__, bool(rejected)
        print("".join(traceback.format_list(frames) + traceback.format_exception_only(type(e), e)), end="")
        applied = [f"{a['tool']} {a['path']}" for a in _report.get("actions", []) if a.get("ok") and a.get("path")]
        if applied:
            print(f"[applied before the error: {', '.join(applied)}; inspect before repeating them]")
    shadowed = [name for name, tool in TOOLS.items() if G.get(name) is not tool]
    if shadowed:
        _report["shadowed"] = shadowed
        print(f"[{', '.join(shadowed)}: low's helpers, restored before the next run]")
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    # Before the reap, which would leave a stopped process looking as if it exited 0.
    for entry, p in _spawned:
        code = p.poll()
        entry[1] = "stopped at the run's end" if code is None else code
    _spawned.clear()
    left = _reap()
    if left:
        _say(f"[stopped {left} process{'es' if left > 1 else ''} this run left running: what a run starts ends with it]")
    if _report.get("check") and _tree() != _checked:
        _report["check"] = False
        print("[check no longer applies: files changed after it passed; run check() again]")
    sys.stdout.flush()
    if "finish" in _report:
        _report["own"] = os.fstat(capture.fileno()).st_size - _said
    with _flow:
        _running = False
        os.dup2(_null, 1)
        os.dup2(_null, 2)
    text = ANSI.sub("", _read(capture).decode("utf-8", "replace"))
    if _dropped:
        text += f"\n[the run printed more than {OUT_CAP >> 20} MB; the rest was dropped]"
    capture.close()
    _report["chars"] = len(text)
    if len(text) > 2 * HALF:
        spill = _spill(text, n)
        where = f"; all {len(text)} are in {spill}: show() or grep it" if spill else ""
        text = f"{text[:HALF]}\n[… {len(text) - 2 * HALF} characters omitted{where}]\n{text[-HALF:]}"
    _outbox.write(json.dumps(dict(_report, out=text or "(no output)", error=failed)) + "\n")
    _outbox.flush()
_kill()
