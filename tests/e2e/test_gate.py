"""A read-only turn while another writer changes the tree does not count as changed: the configured check is not run
against someone else's half-written files and no 'fix what fails' nudge is sent. The turn's own change still is."""
from harness import Env, check, done, user_messages

e = Env([{'code': "print(open('README.md').read())"},
         {'delay': 1.6, 'touch': ['other.txt', 'half-written\n'], 'content': 'README says hello'}],
        low_toml='check = "exit 1"\n')
r = e.low('--json', 'what does the README say?')
gate = [m for q in r['requests'] for m in user_messages(q) if 'The project check failed' in m]
check('no check gate for another writer\'s change', not gate, gate)
check('verdict is NO CHANGES', r['result'].get('verdict') == 'NO CHANGES', r['result'])
check('exit 0', r['code'] == 0, r['err'][-300:])
check('elsewhere counts the other file', r['result'].get('elsewhere') == 1, r['result'].get('elsewhere'))

e = Env([{'code': "write('mine.txt', 'mine\\n')"}, {'content': 'wrote it'}, {'content': 'still failing'}],
        low_toml='check = "exit 1"\n')
r = e.low('--json', 'write mine.txt')
gate = [m for m in user_messages(r['requests'][-1]) if 'The project check failed' in m]
check('own edit still goes through the gate', len(gate) == 1, [user_messages(q)[-1][:80] for q in r['requests']])
check('own edit is UNVERIFIED with a failing check', r['result'].get('verdict') == 'UNVERIFIED', r['result'])
done()
