"""A session's .index left without its process is swept when a session is created; one whose log another process
holds locked stays."""
import fcntl, os
from harness import Env, check, done

e = Env([{'content': 'done'}])
d = os.path.join(e.home, '.low/sessions')
os.makedirs(d)
for n in ('111-1', '222-2'):
    open(os.path.join(d, n + '.jsonl'), 'w').write('{}\n')
    open(os.path.join(d, n + '.index'), 'w').write('x')
held = open(os.path.join(d, '222-2.jsonl'), 'a')
fcntl.flock(held, fcntl.LOCK_EX)
r = e.low('--json', 'task')
check('the run completes', r['code'] == 0, r['err'][-300:])
check('an orphan .index is swept', not os.path.exists(os.path.join(d, '111-1.index')))
check("a locked session's .index stays", os.path.exists(os.path.join(d, '222-2.index')))
done()
