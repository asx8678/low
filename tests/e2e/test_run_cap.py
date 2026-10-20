"""The worker's time cap, driven directly with RUN_CAP patched to 1 s and RUN_MAX to 4 s (the alarm fires 5 s past the
deadline): a busy loop is stopped, one that catches Exception too, and the worker lives on with its variables; the wait
for a check() the host runs does not count; an sh() command whose timeout runs past the cap gets it, as far as RUN_MAX,
and hits its own timeout first. A loop that catches the stop itself is stopped a second later (on Python 3.12+, which
has sys.monitoring), and a command run after a caught stop leaves the alarm alone. It runs this checkout's
src/worker.py, not the one built into $LOW_BIN, on the newest python3.N on PATH, as low does."""
import glob, json, os, re, subprocess, sys, tempfile, time
from harness import ROOT, check, done


def newest_python():
    # The interpreter low runs the worker on (worker.rs's python(), virtualenvs aside): the newest python3.N on PATH.
    # python3 can be macOS's 3.9, which has no sys.monitoring to stop a run that caught the stop.
    found = [(int(m[1]), path) for folder in os.environ.get('PATH', '').split(os.pathsep)
             for path in glob.glob(os.path.join(folder, 'python3.*'))
             if (m := re.fullmatch(r'python3\.(\d+)', os.path.basename(path))) and os.access(path, os.X_OK)]
    return max(found, key=lambda f: f[0])[1] if found else sys.executable


d = tempfile.mkdtemp(prefix='low-e2e-cap-')
source = open(os.path.join(ROOT, 'src/worker.py')).read()
source = re.sub(r'(?m)^RUN_CAP = \d+', 'RUN_CAP = 1', re.sub(r'(?m)^RUN_MAX = \d+', 'RUN_MAX = 4', source))
open(os.path.join(d, 'worker.py'), 'w').write(source)
os.makedirs(os.path.join(d, 'repo'))
subprocess.run(['git', 'init', '-q'], cwd=os.path.join(d, 'repo'), check=True)
p = subprocess.Popen([newest_python(), os.path.join(d, 'worker.py')], cwd=os.path.join(d, 'repo'), text=True,
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=dict(os.environ, LOW_CHECK='true'),
                     start_new_session=True)
ready = json.loads(p.stdout.readline())
check('the worker starts', ready.get('ready'))


def run(code, check_delay=0):
    began = time.time()
    p.stdin.write(json.dumps({'code': code}) + '\n')
    p.stdin.flush()
    while True:
        r = json.loads(p.stdout.readline())
        if not r.get('needs_check'):
            return time.time() - began, r.get('exception'), r['out']
        time.sleep(check_delay)
        p.stdin.write(json.dumps({'ok': True, 'out': '[check passed]'}) + '\n')
        p.stdin.flush()


took, exc, out = run('x = 41\nwhile True: pass')
check('a busy loop is stopped at the cap', exc == 'RunTimeout' and 'past its 1 s limit' in out and took < 10, (took, out))
check('the worker keeps its variables', run('x + 1')[2] == '42\n')
took, exc, out = run('n = 0\nwhile True:\n    try:\n        while True: pass\n    except Exception:\n        n += 1')
check('a loop that catches Exception is stopped too', exc == 'RunTimeout' and took < 10, (took, exc, out))
took, exc, out = run('ok = check()\nprint("after check", ok)', check_delay=7)
check('a slow check() is not cut by the alarm', exc is None and 'after check True' in out, (took, exc, out))
took, exc, out = run('check()\nwhile True: pass', check_delay=7)
check('the alarm is armed again after check()', exc == 'RunTimeout' and took < 16, (took, exc, out))
took, exc, out = run('print(sh("sleep 2", timeout=3))\nprint("done")')
check("a command given longer than the cap finishes", exc is None and out == '\ndone\n' and 2 <= took < 4, (took, out))
took, exc, out = run('print(sh("sleep 30"))')
check("a command's timeout stops at RUN_MAX, before the alarm", exc is None and 'exit timeout after 3s' in out
      and took < 6, (took, out))
took, exc, out = run('sh("sleep 2", timeout=3)\nwhile True: pass')
check('code after a longer command is stopped at the moved deadline', exc == 'RunTimeout'
      and 'past its 3 s limit' in out
      and 'sh(cmd, timeout=...) gives a command longer, up to 4 s a run' in out and 7 < took < 10, (took, out))
if tuple(map(int, ready['python'].split('.')[:2])) >= (3, 12):
    took, exc, out = run('n = 0\nwhile True:\n    try:\n        while True: pass\n'
                         '    except BaseException:\n        n += 1')
    check('a loop that catches the stop itself is stopped a second later', exc == 'RunTimeout' and took < 10,
          (took, exc, out))
else:
    print(f"SKIP a loop that catches the stop itself: the worker's Python {ready['python']} has no sys.monitoring")
took, exc, out = run('sh("sleep 2", timeout=5)\ntry:\n    while True: pass\nexcept:\n    print(sh("echo after"))')
check('a command after a caught stop leaves the alarm alone', exc is None and 'after' in out and 8 < took < 12,
      (took, exc, out))
check('the worker is still alive', run('print("still here")')[2] == 'still here\n')
p.stdin.close()
p.wait(timeout=10)
done()
