"""A check that changes the working tree fails, and its FAILED line names what it changed: a new file, a tracked one,
or, for an untracked file too big for git's index, what kind of file it was. The gate's nudge carries the name too."""
from harness import Env, check, done, user_messages

STEPS = [{'code': "write('a.txt', 'a\\n')\nprint(check())"}, {'content': 'done'}, {'content': 'still'}]

e = Env(STEPS, low_toml='check = "echo s >> scratch.txt"\n')
r = e.low('--json', 'write a.txt')
first = (e.tools() + [''])[0]
check('the new file is named', '[check FAILED: the check changed the working tree: scratch.txt (new)]' in first, first)
gate = [m for q in r['requests'] for m in user_messages(q) if 'The project check failed' in m]
check('the gate nudge names it too', bool(gate) and 'changed the working tree: scratch.txt]' in gate[-1], gate)
check('the verdict stays UNVERIFIED', r['result'].get('verdict') == 'UNVERIFIED' and r['code'] == 1, r['result'])

e = Env(STEPS, low_toml='check = "echo m >> README.md"\n')
e.low('--json', 'write a.txt')
first = (e.tools() + [''])[0]
check('a tracked file is named as it is', '[check FAILED: the check changed the working tree: README.md]' in first, first)

e = Env(STEPS, low_toml='check = "head -c 17000000 /dev/zero > big.bin"\n')
e.low('--json', 'write a.txt')
first = (e.tools() + [''])[0]
check('a file too big to index still gets a reason',
      'the check changed the working tree: an untracked file over 16 MB, which low compares by size and time]' in first,
      first)
done()
