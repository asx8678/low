"""A program driving low --serve watches it through status.json in the inbox: while a task runs the file names the
task, the step and the run under way, the plan, the requests so far and the steers waiting; between tasks it says idle
and how the last task ended. low --send /status prints it. A task's result counts the steers a request took and the
ones that came too late for one."""
import glob, json, os, subprocess, time
from harness import Env, LOW, check, done

e = Env([
    {'code': "plan(['look', 'change'])\nimport time\ntime.sleep(3)\nprint('slept')"}, {'content': 'done one'},
    {'content': 'done two', 'delay': 3},  # a steer sent while this last request runs comes too late
])
server = subprocess.Popen([LOW, '--serve'], cwd=e.repo, env=e.env(), stdin=subprocess.DEVNULL,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def status_file():
    found = glob.glob(os.path.join(e.home, '.low/sessions/*.inbox/status.json'))
    return found[0] if found else None


def status():
    path = status_file()
    try:
        return json.load(open(path)) if path else {}
    except (OSError, ValueError):
        return {}


def wait(cond, timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        s = status()
        if cond(s):
            return s
        time.sleep(0.05)
    return status()


def send(task):
    return subprocess.Popen([LOW, '--send', '--json', task], cwd=e.repo, env=e.env(), stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True)


def result(p, timeout=60):
    out, _ = p.communicate(timeout=timeout)
    return p.returncode, json.loads(out.strip().splitlines()[-1])


try:
    s = wait(lambda s: s.get('state') == 'idle')
    check('a server that has run no task says idle', s.get('state') == 'idle' and s.get('tasks') == 0, s)

    first = send('one')
    s = wait(lambda s: s.get('running'))
    check('a running task names itself, its step and the run under way',
          s.get('state') == 'running' and s.get('task') == 'one' and s.get('step') == 1
          and s['running'].get('ms', -1) >= 0 and s.get('requests') == 1 and s['tokens']['prompt'] > 0, s)
    check('it has a heartbeat and a time it last moved', s.get('at', 0) >= s.get('moved', 1) > 0 and s.get('pid'), s)
    later = wait(lambda x: x.get('at', 0) > s['at'] + 900, timeout=5)
    check('the heartbeat goes on during a long run', later.get('at', 0) > s['at'] and later.get('moved') == s['moved'],
          (s, later))
    p = subprocess.run([LOW, '--send', '--json', '/status'], cwd=e.repo, env=e.env(), capture_output=True, text=True)
    printed = json.loads(p.stdout)
    check('low --send /status prints it', p.returncode == 0 and printed.get('state') == 'running', p.stdout + p.stderr)

    code, r = result(send('/steer mind the tests'), timeout=10)
    s = wait(lambda s: s.get('steers', {}).get('pending') == 1, timeout=5)
    check('a steer waits for the next request', s.get('steers') == {'pending': 1, 'taken': 0}, s)
    code, r = result(first)
    check('the first task finishes', code == 0 and 'done one' in json.dumps(r), r)
    check('its result counts the steer a request took', r.get('steers') == {'taken': 1, 'dropped': 0}, r)
    s = wait(lambda s: s.get('state') == 'idle' and s.get('tasks') == 1)
    check('then the server is idle and says how the task ended',
          s.get('tasks') == 1 and s.get('last', {}).get('exit') == 0 and s['last'].get('status') == 'done', s)

    second = send('two')
    wait(lambda s: s.get('state') == 'running' and s.get('task') == 'two')
    time.sleep(0.5)
    result(send('/steer too late'), timeout=10)
    code, r = result(second)
    check('a steer no request took is counted as dropped', code == 0 and r.get('steers') == {'taken': 0, 'dropped': 1},
          r)
    s = wait(lambda s: s.get('state') == 'idle' and s.get('tasks') == 2)
    check('a new task starts its progress afresh', s.get('tasks') == 2, s)
finally:
    subprocess.run([LOW, '--send', '/stop'], cwd=e.repo, env=e.env(), capture_output=True, timeout=30)
    try:
        server.wait(timeout=15)
    except subprocess.TimeoutExpired:
        server.kill()
check('the status file goes with the server', status_file() is None, status_file())
done()
