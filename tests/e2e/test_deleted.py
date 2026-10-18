"""A file the model deleted with a shell command is not a stray file to clean up: no nudge for it; a stray file made
the same way still gets one, without the deleted file in it."""
from harness import Env, check, done, user_messages

e = Env([{'code': 'sh("rm notes.txt")'}, {'content': 'removed'}, {'content': 'removed, after a nudge'}],
        files={'notes.txt': 'old\n'})
r = e.low('--json', 'remove notes.txt')
check('a deletion alone gets no nudge', r['result'].get('answer') == 'removed' and len(r['requests']) == 2, r['result'])
check('and is reported', r['result'].get('changes') == ['notes.txt (deleted)'], r['result'].get('changes'))

e = Env([{'code': 'sh("rm notes.txt; echo x > out.log")'}, {'content': 'removed'}, {'content': 'kept out.log'}],
        files={'notes.txt': 'old\n'})
r = e.low('--json', 'remove notes.txt')
nudge = [m for q in r['requests'] for m in user_messages(q) if 'without edit() or write()' in m]
check('a stray file still gets the nudge', len(nudge) == 1 and 'out.log' in nudge[0], nudge)
check('without the deleted file in it', bool(nudge) and 'notes.txt' not in nudge[0], nudge)
done()
