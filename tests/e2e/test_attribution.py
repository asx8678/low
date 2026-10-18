"""A file another writer adds during the turn (outside the model's runs) is neither offered for deletion nor reported
as the turn's change; while another low holds a session of the repo, the delete-scratch nudge is skipped."""
import json, os, subprocess, sys, time
from harness import Env, check, done, user_messages

# 1: the run leaves scratch.txt behind. 2: 1.6 s later another writer adds other.txt, then the model answers.
# 3/4: the model removes its scratch and answers again.
SCRIPT = [
    {'code': "open('scratch.txt', 'w').write('x')"},
    {'delay': 1.6, 'touch': ['other.txt', "someone else's work\n"], 'content': 'done'},
    {'delay': 1.0, 'code': "import os; os.remove('scratch.txt')"},
    {'content': 'cleaned up'},
]
e = Env(SCRIPT)
r = e.low('--json', 'make a scratch file')
asks = [m for m in user_messages(r['requests'][-1]) if 'without edit() or write()' in m] if r['requests'] else []
check('solo: the stray nudge fires once', len(asks) == 1, r['err'][-400:])
check('solo: it names the scratch file', bool(asks) and 'scratch.txt' in asks[0], asks)
check("solo: it does not name the other writer's file", bool(asks) and 'other.txt' not in asks[0], asks)
changes = json.dumps(r['result'].get('changes'))
check('solo: other.txt is not in the turn\'s changes', 'other.txt' not in changes, changes)
check('solo: it is counted as elsewhere', r['result'].get('elsewhere') == 1, r['result'].get('elsewhere'))
check('solo: other.txt still exists', os.path.exists(os.path.join(e.repo, 'other.txt')))

# Another low with a session of the same repo: a session log whose header names it, held locked.
e = Env(SCRIPT[:2])
sessions = os.path.join(e.home, '.low/sessions')
os.makedirs(sessions)
path = os.path.join(sessions, '1000-1.jsonl')
open(path, 'w').write(json.dumps({'id': '1000-1', 'cwd': e.repo, 'model': 'mock-model'}) + '\n')
hold = f'import fcntl, time; f = open({path!r}, "a"); fcntl.flock(f, fcntl.LOCK_EX); time.sleep(120)'
holder = subprocess.Popen([sys.executable, '-c', hold])
time.sleep(0.3)
r = e.low('--json', 'make a scratch file')
holder.kill()
asks = [m for q in r['requests'] for m in user_messages(q) if 'without edit() or write()' in m]
changes = json.dumps(r['result'].get('changes'))
check('shared: no delete-scratch nudge while another low works here', not asks, asks)
check('shared: the turn still reports its own scratch file', 'scratch.txt' in changes, r['result'])
check("shared: and not the other writer's file", 'other.txt' not in changes, changes)
done()
