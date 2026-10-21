"""A program driving low --serve steers and stops its running task with low --send "/steer <text>" and
low --send /abort: both return at once, a steer goes with the turn's next request as a REPL steer does, an abort stops
the turn as Ctrl-C does, and the server takes the next task. Neither carries over to the next task, and one sent
while no task runs is dropped."""
import json, os, subprocess, time
from harness import Env, LOW, check, done

SLEEP = "import time\ntime.sleep({})\nprint('slept')"
e = Env([
    {'code': SLEEP.format(3)}, {'content': 'done one'},  # steered during its run
    {'code': SLEEP.format(30)},  # aborted during its run
    {'content': 'done three'},
])
server = subprocess.Popen([LOW, '--serve'], cwd=e.repo, env=e.env(), stdin=subprocess.DEVNULL,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
sessions = os.path.join(e.home, '.low/sessions')
for _ in range(100):
    if os.path.isdir(sessions) and any(f.endswith('.inbox') for f in os.listdir(sessions)):
        break
    time.sleep(0.1)


def send(task):
    return subprocess.Popen([LOW, '--send', '--json', task], cwd=e.repo, env=e.env(), stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True)


def result(p, timeout=60):
    out, _ = p.communicate(timeout=timeout)
    return p.returncode, json.loads(out.strip().splitlines()[-1])


def requests(n, timeout=20):
    end = time.time() + timeout
    while len(e.entries()) < n and time.time() < end:
        time.sleep(0.05)
    return len(e.entries())


try:
    first = send('one')
    requests(1)
    t = time.time()
    code, r = result(send('/steer use the faster path'), timeout=10)
    check('/steer returns at once', code == 0 and r['status'] == 'steer sent' and time.time() - t < 5, r)
    code, r = result(first)
    check('the steered task finishes', code == 0 and 'done one' in json.dumps(r), r)
    users = [(x['content'], x.get('meta', {}).get('steer')) for x in e.log() if x.get('role') == 'user']
    check('the steer went with the next request as a steer', ('use the faster path', True) in users, users)

    second = send('two')
    requests(3)
    t = time.time()
    code, r = result(send('/abort'), timeout=10)
    check('/abort returns at once', code == 0 and r['status'] == 'abort sent', r)
    code, r = result(second, timeout=30)
    check('the aborted task stops well before its 30 s run ends', code == 5 and time.time() - t < 20, (code, r))

    # An abort sent while no task runs, as one sent just as a task ends is, must not stop the next task.
    code, r = result(send('/abort'), timeout=10)
    time.sleep(0.6)
    code, r = result(send('three'))
    check('the next task runs and an abort sent while idle does not stop it', code == 0 and 'done three' in json.dumps(r), (code, r))

    p = subprocess.run([LOW, '--send', '/steer'], cwd=e.repo, env=e.env(), capture_output=True, text=True)
    check('/steer without a message is refused', p.returncode != 0 and 'needs a message' in p.stderr, p.stderr)
finally:
    subprocess.run([LOW, '--send', '/stop'], cwd=e.repo, env=e.env(), capture_output=True, timeout=30)
    try:
        server.wait(timeout=15)
    except subprocess.TimeoutExpired:
        server.kill()
done()
