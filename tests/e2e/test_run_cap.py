"""The worker's time cap, driven directly with RUN_CAP patched to 1 s (the alarm fires 5 s past it): a busy loop is
stopped and the worker lives on with its variables; the wait for a check() the host runs does not count; sh() hits
its own timeout first. It runs this checkout's src/worker.py, not the one built into $LOW_BIN."""
import json, os, re, subprocess, sys, tempfile, time
from harness import ROOT, check, done

d = tempfile.mkdtemp(prefix='low-e2e-cap-')
source = open(os.path.join(ROOT, 'src/worker.py')).read()
open(os.path.join(d, 'worker.py'), 'w').write(re.sub(r'(?m)^RUN_CAP = \d+$', 'RUN_CAP = 1', source))
os.makedirs(os.path.join(d, 'repo'))
subprocess.run(['git', 'init', '-q'], cwd=os.path.join(d, 'repo'), check=True)
p = subprocess.Popen([sys.executable, os.path.join(d, 'worker.py')], cwd=os.path.join(d, 'repo'), text=True,
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=dict(os.environ, LOW_CHECK='true'),
                     start_new_session=True)
check('the worker starts', json.loads(p.stdout.readline()).get('ready'))


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
check('a busy loop is stopped at the cap', exc == 'TimeoutError' and 'past its 1 s limit' in out and took < 10, (took, out))
check('the worker keeps its variables', run('x + 1')[2] == '42\n')
took, exc, out = run('ok = check()\nprint("after check", ok)', check_delay=7)
check('a slow check() is not cut by the alarm', exc is None and 'after check True' in out, (took, exc, out))
took, exc, out = run('check()\nwhile True: pass', check_delay=7)
check('the alarm is armed again after check()', exc == 'TimeoutError' and took < 16, (took, exc, out))
took, exc, out = run('print(sh("sleep 30"))')
check("sh() hits its own timeout first", exc is None and 'exit timeout after 1s' in out, (took, out))
check('the worker is still alive', run('print("still here")')[2] == 'still here\n')
p.stdin.close()
p.wait(timeout=10)
done()
