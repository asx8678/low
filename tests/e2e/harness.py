"""Shared helpers for the end-to-end tests: a throwaway HOME and git repo, the scripted provider in mock.py, and low
itself. Stdlib only; nothing here touches the real ~/.low or ~/.config/low, and no request leaves the machine."""
import atexit, json, os, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
# The binary under test: $LOW_BIN, else this checkout's debug build (the one low.toml's check builds).
LOW = os.environ.get('LOW_BIN') or os.path.join(ROOT, 'target/debug/low')
KEY = 'sk-mocktestkey000000000000000000000001'
_envs, _failed = [], []


class Env:
    """One HOME, one repo and one mock serving `script` to every low run in it, in order."""

    def __init__(self, script, files=None, profile='', config='', low_toml='', git=True):
        self.dir = os.path.realpath(tempfile.mkdtemp(prefix='low-e2e-'))
        self.home, self.repo = os.path.join(self.dir, 'home'), os.path.join(self.dir, 'repo')
        os.makedirs(os.path.join(self.home, '.config/low'))
        os.makedirs(self.repo)
        self.record, scriptfile, portfile = (os.path.join(self.dir, n) for n in ('requests.jsonl', 'script.json', 'port'))
        json.dump(script, open(scriptfile, 'w'))
        self.mock = subprocess.Popen([sys.executable, '-I', os.path.join(HERE, 'mock.py'), scriptfile, portfile,
                                      self.record, self.repo])
        _envs.append(self)
        for _ in range(200):
            if os.path.exists(portfile) and open(portfile).read():
                break
            time.sleep(0.05)
        self.port = int(open(portfile).read())
        url = f'http://127.0.0.1:{self.port}/v1'
        self.write_home('.config/low/providers.toml', f'[mock]\nbase_url = "{url}"\napi_key = "{KEY}"\n'
                        f'models = ["mock-model"]\n[spare]\nbase_url = "{url}"\napi_key = "{KEY}"\nmodels = ["spare-model"]\n')
        self.write_home('.config/low/config.toml', f'model = "mock-model"\n{config}\n[models."mock-model"]\n{profile}\n')
        for rel, text in (files if files is not None else {'README.md': 'hello\n'}).items():
            self.write(rel, text)
        if low_toml:
            self.write('low.toml', low_toml)
        if git:
            self.git('init', '-q', '-b', 'main')
            self.git('add', '-A')
            self.git('commit', '-qm', 'init')

    def write(self, rel, text, root=None):
        path = os.path.join(root or self.repo, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        open(path, 'wb' if isinstance(text, bytes) else 'w').write(text)

    def write_home(self, rel, text):
        self.write(rel, text, self.home)

    def read(self, rel):
        return open(os.path.join(self.repo, rel)).read()

    def git(self, *args, input=None):
        ident = ['-c', 'user.email=t@t', '-c', 'user.name=t']
        out = subprocess.run(['git', *ident, *args], cwd=self.repo, env=self.env(), capture_output=True, text=True,
                             input=input)
        return out.stdout.strip()

    def env(self, extra=None):
        env = {k: v for k, v in os.environ.items() if k in ('PATH', 'TERM', 'TMPDIR', 'USER', 'LANG')}
        return {**env, 'HOME': self.home, **(extra or {})}

    def low(self, *args, timeout=300, env=None, cwd=None):
        """Run low in the repo, or in `cwd`. Returns its exit code, output, the last stdout line parsed as JSON
        (--json's result), the requests the mock saw during this run and, apart, its second looks' requests."""
        before, looked = len(self.entries()), len(self.asides())
        p = subprocess.run([LOW, *args], cwd=cwd or self.repo, env=self.env(env), capture_output=True, text=True,
                           stdin=subprocess.DEVNULL, timeout=timeout)
        try:
            result = json.loads(p.stdout.strip().splitlines()[-1])
        except (ValueError, IndexError):
            result = {}
        return {'code': p.returncode, 'out': p.stdout, 'err': p.stderr, 'result': result,
                'requests': [e['body'] for e in self.entries()[before:]],
                'asides': [e['body'] for e in self.asides()[looked:]]}

    def entries(self):
        """What the mock recorded of every request: i, at, auth, body (parsed) and raw (the bytes sent). A second
        look's requests are in asides() instead."""
        return [x for x in self._recorded() if not x.get('aside')]

    def asides(self):
        """What the mock recorded of the requests of low's second looks at a turn's claims."""
        return [x for x in self._recorded() if x.get('aside')]

    def _recorded(self):
        return [json.loads(l) for l in open(self.record)] if os.path.exists(self.record) else []

    def sessions(self):
        d = os.path.join(self.home, '.low/sessions')
        return sorted(os.path.join(d, f) for f in os.listdir(d) if f.endswith('.jsonl')) if os.path.isdir(d) else []

    def log(self):
        """The latest session's log entries."""
        sessions = self.sessions()
        return [json.loads(l) for l in open(sessions[-1])] if sessions else []

    def tools(self):
        """The tool results of the latest session, as the model got them."""
        return [x['content'] for x in self.log() if x.get('role') == 'tool']


def user_messages(body):
    return [m['content'] for m in body['messages'] if m['role'] == 'user' and isinstance(m.get('content'), str)]


def check(name, ok, detail=''):
    print(('PASS ' if ok else 'FAIL ') + name + (f'  [{str(detail)[:600]}]' if detail and not ok else ''), flush=True)
    if not ok:
        _failed.append(name)
    return ok


def done():
    sys.exit(1 if _failed else 0)


@atexit.register
def _close():
    for e in _envs:
        e.mock.kill()
        e.mock.wait()
        if not os.environ.get('E2E_KEEP'):
            shutil.rmtree(e.dir, ignore_errors=True)
