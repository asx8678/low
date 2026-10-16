import ast, contextlib, ctypes, difflib, importlib.util, json, linecache, os, re, shlex, signal, stat, subprocess, sys, tempfile, threading, time, traceback

ROOT = os.getcwd()
sys.path.append(ROOT)
CHECK = os.environ.pop("AIR_CHECK", "")
FMT = os.environ.pop("AIR_FMT", "")
HALF = 16000
LIMIT = 8 << 20
RUN_CAP = 570
IGNORABLE = re.compile("[\u200c-\u200f\u202a-\u202e\u206a-\u206f\ufeff]")
ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\r(?!\n)")

_inbox = os.fdopen(os.dup(0), encoding="utf-8")
_outbox = os.fdopen(os.dup(1), "w", encoding="utf-8")
_null = os.open(os.devnull, os.O_RDWR)
os.dup2(_null, 0)
_report = {}
_state = {}
_checked = None
_groups = set()
_deadline = time.monotonic() + RUN_CAP


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
    os.killpg(0, signal.SIGKILL)


def _watch(parent):
    while os.getppid() == parent:
        time.sleep(1)
    _kill()


threading.Thread(target=_watch, args=(os.getppid(),), daemon=True).start()


def _exec(cmd, timeout):
    left = _deadline - time.monotonic()
    if not timeout or timeout > left:
        timeout = max(1, int(left))
    with tempfile.TemporaryFile() as out:
        p = subprocess.Popen(cmd, shell=True, cwd=ROOT, stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
                             start_new_session=True)
        _groups.add(p.pid)
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
        _groups.discard(p.pid)
        return ANSI.sub("", _read(out).decode("utf-8", "replace")), code


def _read(f):
    size = f.seek(0, 2)
    f.seek(0)
    if size <= LIMIT:
        return f.read()
    head = f.read(LIMIT // 2)
    f.seek(-LIMIT // 2, 2)
    return head + f"\n[… {size - LIMIT} bytes omitted]\n".encode() + f.read()


def _rel(path):
    return os.path.relpath(os.path.join(ROOT, path), ROOT)


def _protected():
    hooks = subprocess.run(["git", "config", "core.hooksPath"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    dirs = [os.path.join(ROOT, ".git")]
    if hooks:
        hooks = os.path.realpath(os.path.join(ROOT, os.path.expanduser(hooks)))
        dirs.append(os.path.dirname(hooks) if os.path.basename(hooks) == "_" else hooks)
    return [os.path.realpath(d) for d in dirs]


PROTECTED = _protected()


def _change(tool, path, make):
    rel = _rel(path)
    real = os.path.realpath(os.path.join(ROOT, rel))
    signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT})
    try:
        if rel.startswith("..") or not real.startswith(ROOT + os.sep):
            raise ValueError(f"{path} is outside the repo")
        parts = (rel + os.sep + os.path.relpath(real, ROOT)).split(os.sep)
        if any(d == real or real.startswith(d + os.sep) for d in PROTECTED) or any(
                IGNORABLE.sub("", p).casefold() == ".git" for p in parts):
            raise ValueError(f"{path} is off limits: git runs code from there outside the sandbox")
        st = os.stat(real) if os.path.lexists(real) else None
        if st and stat.S_ISREG(st.st_mode) and (st.st_nlink > 1 or st.st_dev != os.stat(ROOT).st_dev):
            raise ValueError(f"{path} has other hard links or lives on another volume; air will not change it")
        if not (_report.get("accepted") or _state.get("accepted")):
            raise ValueError("call accept(criteria) before changing files")
        before, written = make(os.path.join(ROOT, rel), rel)
        delta = [l[0] for l in difflib.unified_diff(before.splitlines(), written.splitlines(), lineterm="")][2:]
        _report.setdefault("actions", []).append(
            {"tool": tool, "path": rel, "ok": True, "added": delta.count("+"), "removed": delta.count("-")})
    except Exception as e:
        _report.setdefault("actions", []).append({"tool": tool, "path": rel, "ok": False, "error": str(e)})
        raise
    finally:
        signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGINT})
    _forget()
    note = ""
    if FMT:
        out, code = _exec(f"{FMT} {shlex.quote('./' + rel if rel.startswith('-') else rel)}", 60)
        note = f"; fmt failed: {out.strip().splitlines()[0] if out.strip() else code}" if code else ""
    with open(os.path.join(ROOT, rel), encoding="utf-8") as f:
        after = f.read()
    written = written.replace("\r\n", "\n")
    if after != written:
        diff = difflib.unified_diff(written.splitlines(True), after.splitlines(True), n=1)
        note += "; the formatter changed it:\n" + "".join(list(diff)[2:]).rstrip("\n")
    print(f"{tool} {rel}: ok{note}")


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


def edit(path, old, new=None):
    if new is None and isinstance(old, (list, tuple)) and len(old) == 2 and all(isinstance(s, str) for s in old):
        old, new = old  # one pair given as [old, new]
    pairs = [(old, new)] if new is not None else old
    if isinstance(pairs, str) or not pairs or not all(
            isinstance(p, (list, tuple)) and len(p) == 2 and all(isinstance(s, str) for s in p) for p in pairs):
        raise TypeError("edit() takes (path, old, new) or (path, [(old, new), ...]) with string old and new")
    def make(full, rel):
        with open(full, encoding="utf-8", newline="") as f:
            raw = f.read()
        crlf = "\r\n" in raw and raw.count("\r\n") == raw.count("\n")
        eol = (lambda s: s.replace("\r\n", "\n")) if crlf else (lambda s: s)
        text = result = eol(raw)
        for k, (o, n) in enumerate(pairs, 1):
            find = eol(o)
            count = result.count(find)
            if count != 1:
                which = f" (pair {k} of {len(pairs)}; nothing was changed)" if len(pairs) > 1 else ""
                raise ValueError(f"old text {'not found' if count == 0 else f'occurs {count} times'} in {rel}{which}")
            result = result.replace(find, eol(n), 1)
        data = (result.replace("\n", "\r\n") if crlf else result).encode("utf-8")
        with open(full, "wb") as f:
            f.write(data)
        return text, result
    _change("edit", path, make)


def write(path, content):
    def make(full, rel):
        if os.path.lexists(full):
            raise ValueError(f"{rel} exists; use edit")
        data = content.encode("utf-8")
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "xb") as f:
            f.write(data)
        return "", content
    _change("write", path, make)


def sh(cmd, timeout=120):
    entry = [str(cmd), None, []]
    _report.setdefault("cmds", []).append(entry)
    out, code = _exec(cmd, timeout)
    entry[1:] = [code, [_rel(w) for w in str(cmd).split() if os.path.isfile(os.path.join(ROOT, w))]]
    return out.rstrip("\n") + (f"\n[exit {code}]" if code else "")


def show(path, start=1, end=None):
    with open(os.path.join(ROOT, path), encoding="utf-8", errors="replace") as f:
        lines = f.read().splitlines()
    _report.setdefault("reads", []).append(_rel(path))
    asked, start, end = end, max(start, 1), min(end or len(lines), len(lines))
    sys.stdout.flush()
    room, rows = 2 * HALF - os.fstat(1).st_size - 2000, []
    for n in range(start, end + 1):
        room -= len(lines[n - 1]) + len(str(n)) + 3
        if room < 0 and rows:
            break
        rows.append(f"{n}: {lines[n - 1]}")
    stop = start + len(rows) - 1
    rest = f"; show({_rel(path)!r}, {stop + 1}{f', {asked}' if asked else ''}) continues" if stop < end else ""
    print(f"[{_rel(path)}: lines {start}-{stop} of {len(lines)}{rest}]")
    print("\n".join(rows))


def accept(criteria):
    _report["accepted"] = str(criteria)


def check():
    if not CHECK:
        print("no check command configured (set check in air.toml)")
        return None
    ran = len(_report.get("cmds", []))
    _outbox.write(json.dumps({"needs_check": True, "actions": _report.get("actions", []), "cmds": _report.get("cmds", [])}) + "\n")
    _outbox.flush()
    answer = json.loads(_inbox.readline() or "{}")
    ok = answer.get("ok") is True
    global _checked, _deadline
    _deadline = time.monotonic() + RUN_CAP
    _checked = _tree() if ok else None
    _report["check"] = ok
    _report.setdefault("actions", []).append({"tool": "check", "ok": ok, "cmds": ran})
    print(answer.get("out") or f"[check {'passed' if ok else 'FAILED'}]")
    return ok


def _tree():
    out = subprocess.run(["git", "ls-files", "-co", "--exclude-standard", "-z"], cwd=ROOT, capture_output=True).stdout
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


TOOLS = {"sh": sh, "show": show,"edit": edit, "write": write, "accept": accept, "plan": plan, "done": done, "check": check}
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
        print(f"[import {module.__spec__.name}: air's helpers are built in; no import needed]")


sh.sh = sys.modules["sh"] = sh
sys.meta_path.append(_Helpers())
signal.signal(signal.SIGINT, signal.SIG_IGN)
_outbox.write(json.dumps({"ready": True, "python": sys.version.split()[0]}) + "\n")
_outbox.flush()
for request in _inbox:
    request = json.loads(request)
    code, _state = request["code"], request.get("state") or {}
    _deadline = time.monotonic() + RUN_CAP
    _report.clear()
    os.chdir(ROOT)
    G.update(TOOLS)
    capture = tempfile.TemporaryFile()
    os.dup2(capture.fileno(), 1)
    os.dup2(capture.fileno(), 2)
    sys.stdout = sys.stderr = open(1, "w", encoding="utf-8", errors="replace", buffering=1, closefd=False)
    failed = False
    signal.signal(signal.SIGINT, signal.default_int_handler)
    try:
        linecache.cache["<run>"] = (len(code), None, code.splitlines(True), "<run>")
        tree = ast.parse(code, "<run>")
        tail = tree.body.pop() if tree.body and isinstance(tree.body[-1], ast.Expr) else None
        exec(compile(tree, "<run>", "exec"), G)
        if tail:
            value = eval(compile(ast.Expression(tail.value), "<run>", "eval"), G)
            sys.stdout.flush()
            if value is not None and not os.fstat(1).st_size:
                print(value)
    except BaseException as e:
        failed = True
        tb = traceback.extract_tb(e.__traceback__)
        frames = [f for f in tb if f.filename == "<run>"]
        misuse = isinstance(e, TypeError) and tb and tb[-1].filename == "<run>" and str(e).startswith(HELPERS)
        rejected = isinstance(e, (SyntaxError, NameError)) or misuse
        _report["exception"], _report["rejected"] = type(e).__name__, bool(rejected)
        print("".join(traceback.format_list(frames) + traceback.format_exception_only(type(e), e)), end="")
        applied = [f"{a['tool']} {a['path']}" for a in _report.get("actions", []) if a.get("ok") and a.get("path")]
        if applied:
            print(f"[applied before the error: {', '.join(applied)}; inspect before repeating them]")
    shadowed = [name for name, tool in TOOLS.items() if G.get(name) is not tool]
    if shadowed:
        _report["shadowed"] = shadowed
        print(f"[{', '.join(shadowed)}: air's helpers, restored before the next run]")
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    if _report.get("check") and _tree() != _checked:
        _report["check"] = False
        print("[check no longer applies: files changed after it passed; run check() again]")
    sys.stdout.flush()
    os.dup2(_null, 1)
    os.dup2(_null, 2)
    text = ANSI.sub("", _read(capture).decode("utf-8", "replace"))
    capture.close()
    _report["chars"] = len(text)
    if len(text) > 2 * HALF:
        text = f"{text[:HALF]}\n[… {len(text) - 2 * HALF} characters omitted]\n{text[-HALF:]}"
    _outbox.write(json.dumps(dict(_report, out=text or "(no output)", error=failed)) + "\n")
    _outbox.flush()
_kill()
