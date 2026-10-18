"""low --serve takes tasks from low --send, which waits as long as the task runs: a --send killed while it waits (a
`timeout` around it) stops only the waiting, and the server finishes that task and takes the next; /stop ends it."""
import os, subprocess, time
from harness import Env, LOW, check, done

e = Env([{'delay': 4, 'content': 'first, after a while'}, {'content': 'second'}])
server = subprocess.Popen([LOW, '--serve'], cwd=e.repo, env=e.env(), stdin=subprocess.DEVNULL,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
sessions = os.path.join(e.home, '.low/sessions')
for _ in range(100):
    if os.path.isdir(sessions) and any(f.endswith('.inbox') for f in os.listdir(sessions)):
        break
    time.sleep(0.1)
try:
    e.low('--send', '--json', 'one', timeout=1.5)
    waited = False
except subprocess.TimeoutExpired:  # killed, as a timeout around it would
    waited = True
check('--send waits while the task runs', waited)
r = e.low('--send', '--json', 'two', timeout=60)
said = [x.get('content') for x in e.log() if x.get('role') == 'assistant']
check('the server finished the task whose waiter was killed', said[:1] == ['first, after a while'], said)
check('and answered the next --send', 'second' in r['out'] and r['code'] == 0, r['out'][-400:])
r = e.low('--send', '/stop', timeout=30)
try:
    code = server.wait(timeout=15)
except subprocess.TimeoutExpired:
    server.kill()
    code = 'still running'
check('/stop ends the server', r['code'] == 0 and code == 0, (r['code'], code))
done()
